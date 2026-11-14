/*
 * H.264 encoding on the GPU through VA-API.
 *
 * BGRX pixels are uploaded into an RGB VA surface, converted to NV12 by the VA video
 * processor (full-range BT.709, the matrix RDP clients use for AVC420) and encoded by
 * FFmpeg's h264_vaapi. The CPU only copies the pixels into the upload surface.
 *
 * A host GPU buffer (dmabuf) skips the upload: it is imported as the RGB surface itself
 * (once per buffer generation) and the video processor reads it in place, flipping it when
 * its rows are stored bottom-up.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <libavutil/opt.h>
#include <unistd.h>

#include <va/va.h>
#include <va/va_drmcommon.h>
#include <va/va_vpp.h>

/* DRM fourccs (drm_fourcc.h), spelled out to avoid a libdrm build dependency. */
#define VDS_FOURCC(a, b, c, d) \
    ((uint32_t)(a) | ((uint32_t)(b) << 8) | ((uint32_t)(c) << 16) | ((uint32_t)(d) << 24))
#define VDS_DRM_XRGB8888 VDS_FOURCC('X', 'R', '2', '4')
#define VDS_DRM_ARGB8888 VDS_FOURCC('A', 'R', '2', '4')
#define VDS_DRM_XBGR8888 VDS_FOURCC('X', 'B', '2', '4')
#define VDS_DRM_ABGR8888 VDS_FOURCC('A', 'B', '2', '4')

/* A host GPU buffer: one plane, 32 bits per pixel. */
typedef struct vds_vaapi_dmabuf {
    int32_t fd;
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint32_t fourcc;
    uint64_t modifier;
} vds_vaapi_dmabuf;

typedef struct vds_vaapi_config {
    int32_t width;
    int32_t height;
    int32_t fps;
    /* DRM render node, e.g. /dev/dri/renderD128; NULL = FFmpeg's default. */
    const char *render_node;
    /* Bitrate in kbit/s (CBR). */
    int32_t bitrate_kbps;
} vds_vaapi_config;

typedef struct vds_vaapi {
    AVBufferRef *device;
    AVBufferRef *rgb_frames;
    AVBufferRef *nv12_frames;
    AVCodecContext *enc;
    AVPacket *pkt;
    AVFrame *sw;
    VADisplay dpy;
    VAConfigID vpp_config;
    VAContextID vpp_context;
    /* The video processor can flip vertically. */
    int can_flip;
    /* The imported host buffer and its generation (0 = none). */
    VASurfaceID gpu_surface;
    uint64_t gpu_generation;
    int32_t width;
    int32_t height;
    int32_t fps;
    uint8_t *out;
    size_t out_cap;
} vds_vaapi;

static void set_err(char *err, size_t err_len, const char *what, int code)
{
    char buf[AV_ERROR_MAX_STRING_SIZE] = {0};

    if (code < 0) {
        av_strerror(code, buf, sizeof(buf));
        snprintf(err, err_len, "%s: %s", what, buf);
    } else {
        snprintf(err, err_len, "%s", what);
    }
}

static AVBufferRef *frames_new(AVBufferRef *device, enum AVPixelFormat sw_format,
                               int32_t w, int32_t h, int pool, int *ret)
{
    AVBufferRef *ref = av_hwframe_ctx_alloc(device);
    AVHWFramesContext *fc;

    if (!ref) {
        *ret = AVERROR(ENOMEM);
        return NULL;
    }
    fc = (AVHWFramesContext *)ref->data;
    fc->format = AV_PIX_FMT_VAAPI;
    fc->sw_format = sw_format;
    fc->width = w;
    fc->height = h;
    fc->initial_pool_size = pool;
    *ret = av_hwframe_ctx_init(ref);
    if (*ret < 0) {
        av_buffer_unref(&ref);
    }
    return ref;
}

void vds_vaapi_close(vds_vaapi *h);

static int apply_rate(vds_vaapi *h, int32_t bitrate_kbps)
{
    int64_t bps = (int64_t)bitrate_kbps * 1000;

    /* VBR: aim at half the cap, never above it. */
    h->enc->bit_rate = bps / 2;
    h->enc->rc_max_rate = bps;
    /* 1.5 frames of budget: a busy frame may borrow half a frame. */
    h->enc->rc_buffer_size = (int)(bps * 3 / (2 * (h->fps > 0 ? h->fps : 30)));
    return 0;
}

vds_vaapi *vds_vaapi_open(const vds_vaapi_config *cfg, char *err, size_t err_len)
{
    vds_vaapi *h;
    const AVCodec *codec;
    AVVAAPIDeviceContext *va;
    AVDictionary *opts = NULL;
    VAStatus vs;
    int ret;

    h = calloc(1, sizeof(*h));
    if (!h) {
        set_err(err, err_len, "out of memory", 0);
        return NULL;
    }
    h->width = cfg->width;
    h->height = cfg->height;
    h->fps = cfg->fps > 0 ? cfg->fps : 30;
    h->vpp_config = VA_INVALID_ID;
    h->vpp_context = VA_INVALID_ID;
    h->gpu_surface = VA_INVALID_SURFACE;

    ret = av_hwdevice_ctx_create(&h->device, AV_HWDEVICE_TYPE_VAAPI,
                                 cfg->render_node, NULL, 0);
    if (ret < 0) {
        set_err(err, err_len, "cannot open the VA-API device", ret);
        goto fail;
    }
    va = ((AVHWDeviceContext *)h->device->data)->hwctx;
    h->dpy = va->display;

    /* BGRX in memory is FFmpeg's BGR0 (bytes B, G, R, X). */
    h->rgb_frames = frames_new(h->device, AV_PIX_FMT_BGR0, cfg->width, cfg->height, 2, &ret);
    if (!h->rgb_frames) {
        set_err(err, err_len, "cannot create the RGB upload surfaces", ret);
        goto fail;
    }
    h->nv12_frames = frames_new(h->device, AV_PIX_FMT_NV12, cfg->width, cfg->height, 4, &ret);
    if (!h->nv12_frames) {
        set_err(err, err_len, "cannot create the NV12 surfaces", ret);
        goto fail;
    }

    vs = vaCreateConfig(h->dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &h->vpp_config);
    if (vs != VA_STATUS_SUCCESS) {
        snprintf(err, err_len, "VA video processing unavailable: %s", vaErrorStr(vs));
        goto fail;
    }
    vs = vaCreateContext(h->dpy, h->vpp_config, cfg->width, cfg->height, VA_PROGRESSIVE,
                         NULL, 0, &h->vpp_context);
    if (vs != VA_STATUS_SUCCESS) {
        snprintf(err, err_len, "cannot create the VA video processor: %s", vaErrorStr(vs));
        goto fail;
    }
    {
        VAProcPipelineCaps caps;

        memset(&caps, 0, sizeof(caps));
        if (vaQueryVideoProcPipelineCaps(h->dpy, h->vpp_context, NULL, 0, &caps) ==
            VA_STATUS_SUCCESS) {
            h->can_flip = (caps.mirror_flags & VA_MIRROR_VERTICAL) != 0;
        }
    }

    codec = avcodec_find_encoder_by_name("h264_vaapi");
    if (!codec) {
        set_err(err, err_len, "FFmpeg has no h264_vaapi encoder", 0);
        goto fail;
    }
    h->enc = avcodec_alloc_context3(codec);
    if (!h->enc) {
        set_err(err, err_len, "out of memory", 0);
        goto fail;
    }
    h->enc->width = cfg->width;
    h->enc->height = cfg->height;
    h->enc->time_base = (AVRational){1, 1000};
    h->enc->framerate = (AVRational){h->fps, 1};
    h->enc->pix_fmt = AV_PIX_FMT_VAAPI;
    h->enc->hw_frames_ctx = av_buffer_ref(h->nv12_frames);
    /* IDR only on demand (TCP loses nothing), no B-frames, no reordering delay. */
    h->enc->gop_size = INT32_MAX;
    h->enc->max_b_frames = 0;
    h->enc->profile = AV_PROFILE_H264_CONSTRAINED_BASELINE;
    h->enc->flags |= AV_CODEC_FLAG_LOW_DELAY;
    /* Full-range BT.709, what the video processor writes. */
    h->enc->color_range = AVCOL_RANGE_JPEG;
    h->enc->color_primaries = AVCOL_PRI_BT709;
    h->enc->color_trc = AVCOL_TRC_BT709;
    h->enc->colorspace = AVCOL_SPC_BT709;
    apply_rate(h, cfg->bitrate_kbps > 0 ? cfg->bitrate_kbps : 20000);

    av_dict_set(&opts, "rc_mode", "VBR", 0);
    av_dict_set(&opts, "async_depth", "1", 0);
    ret = avcodec_open2(h->enc, codec, &opts);
    av_dict_free(&opts);
    if (ret < 0) {
        set_err(err, err_len, "cannot open h264_vaapi", ret);
        goto fail;
    }

    h->pkt = av_packet_alloc();
    h->sw = av_frame_alloc();
    if (!h->pkt || !h->sw) {
        set_err(err, err_len, "out of memory", 0);
        goto fail;
    }
    return h;

fail:
    vds_vaapi_close(h);
    return NULL;
}

static int32_t encode_nv12(vds_vaapi *h, AVFrame *nv12, int32_t force_idr, int64_t pts_ms,
                           const uint8_t **out);

/* Converts the RGB surface `src` into the NV12 surface `dst` on the GPU, flipped vertically
 * when `flip` is set. */
static int vpp_convert(vds_vaapi *h, VASurfaceID src, VASurfaceID dst, int flip)
{
    VAProcPipelineParameterBuffer p;
    VABufferID buf = VA_INVALID_ID;
    VAStatus vs;

    memset(&p, 0, sizeof(p));
    p.surface = src;
    p.surface_color_standard = VAProcColorStandardBT709;
    p.output_color_standard = VAProcColorStandardBT709;
    p.input_color_properties.color_range = VA_SOURCE_RANGE_FULL;
    p.output_color_properties.color_range = VA_SOURCE_RANGE_FULL;
    p.output_background_color = 0xff000000;
    p.filter_flags = VA_FRAME_PICTURE;
    p.mirror_state = flip ? VA_MIRROR_VERTICAL : VA_MIRROR_NONE;

    vs = vaBeginPicture(h->dpy, h->vpp_context, dst);
    if (vs != VA_STATUS_SUCCESS) {
        return -1;
    }
    vs = vaCreateBuffer(h->dpy, h->vpp_context, VAProcPipelineParameterBufferType,
                        sizeof(p), 1, &p, &buf);
    if (vs == VA_STATUS_SUCCESS) {
        vs = vaRenderPicture(h->dpy, h->vpp_context, &buf, 1);
    }
    if (vaEndPicture(h->dpy, h->vpp_context) != VA_STATUS_SUCCESS) {
        vs = VA_STATUS_ERROR_OPERATION_FAILED;
    }
    if (buf != VA_INVALID_ID) {
        vaDestroyBuffer(h->dpy, buf);
    }
    return vs == VA_STATUS_SUCCESS ? 0 : -1;
}

/*
 * Encodes one `width`x`height` BGRX picture (`stride` bytes per row). On success returns
 * the size of the access unit written to *out (owned by the encoder, valid until the next
 * call), 0 if the encoder produced nothing, or a negative value on error.
 */
int32_t vds_vaapi_encode(vds_vaapi *h, const uint8_t *bgrx, int32_t stride, int32_t force_idr,
                         int64_t pts_ms, const uint8_t **out)
{
    AVFrame *rgb = av_frame_alloc();
    AVFrame *nv12 = av_frame_alloc();
    int32_t size = -1;

    *out = NULL;
    if (!rgb || !nv12) {
        goto done;
    }
    if (av_hwframe_get_buffer(h->rgb_frames, rgb, 0) < 0 ||
        av_hwframe_get_buffer(h->nv12_frames, nv12, 0) < 0) {
        goto done;
    }

    /* Upload: the only pass over the pixels on the CPU. */
    av_frame_unref(h->sw);
    h->sw->format = AV_PIX_FMT_BGR0;
    h->sw->width = h->width;
    h->sw->height = h->height;
    h->sw->data[0] = (uint8_t *)bgrx;
    h->sw->linesize[0] = stride;
    if (av_hwframe_transfer_data(rgb, h->sw, 0) < 0) {
        goto done;
    }
    h->sw->data[0] = NULL;

    if (vpp_convert(h, (VASurfaceID)(uintptr_t)rgb->data[3],
                    (VASurfaceID)(uintptr_t)nv12->data[3], 0) < 0) {
        goto done;
    }
    size = encode_nv12(h, nv12, force_idr, pts_ms, out);

done:
    if (h->sw) {
        h->sw->data[0] = NULL;
    }
    av_frame_free(&rgb);
    av_frame_free(&nv12);
    return size;
}

/* VA fourcc of a DRM fourcc, 0 if the format is not supported. */
static uint32_t va_fourcc(uint32_t drm)
{
    switch (drm) {
    case VDS_DRM_XRGB8888:
        return VA_FOURCC_BGRX;
    case VDS_DRM_ARGB8888:
        return VA_FOURCC_BGRA;
    case VDS_DRM_XBGR8888:
        return VA_FOURCC_RGBX;
    case VDS_DRM_ABGR8888:
        return VA_FOURCC_RGBA;
    default:
        return 0;
    }
}

/* Wraps the host buffer `buf` in a VA surface; the buffer stays the host's. */
static VAStatus import_dmabuf(VADisplay dpy, const vds_vaapi_dmabuf *buf, VASurfaceID *surface)
{
    VADRMPRIMESurfaceDescriptor desc;
    VASurfaceAttrib attrs[2];
    uint32_t fourcc = va_fourcc(buf->fourcc);
    off_t size = lseek(buf->fd, 0, SEEK_END);

    if (!fourcc || size <= 0) {
        return VA_STATUS_ERROR_INVALID_PARAMETER;
    }
    memset(&desc, 0, sizeof(desc));
    desc.fourcc = fourcc;
    desc.width = buf->width;
    desc.height = buf->height;
    desc.num_objects = 1;
    desc.objects[0].fd = buf->fd;
    desc.objects[0].size = (uint32_t)size;
    desc.objects[0].drm_format_modifier = buf->modifier;
    desc.num_layers = 1;
    desc.layers[0].drm_format = buf->fourcc;
    desc.layers[0].num_planes = 1;
    desc.layers[0].object_index[0] = 0;
    desc.layers[0].offset[0] = 0;
    desc.layers[0].pitch[0] = buf->stride;

    memset(attrs, 0, sizeof(attrs));
    attrs[0].type = VASurfaceAttribMemoryType;
    attrs[0].flags = VA_SURFACE_ATTRIB_SETTABLE;
    attrs[0].value.type = VAGenericValueTypeInteger;
    attrs[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
    attrs[1].type = VASurfaceAttribExternalBufferDescriptor;
    attrs[1].flags = VA_SURFACE_ATTRIB_SETTABLE;
    attrs[1].value.type = VAGenericValueTypePointer;
    attrs[1].value.value.p = &desc;
    return vaCreateSurfaces(dpy, VA_RT_FORMAT_RGB32, buf->width, buf->height, surface, 1,
                            attrs, 2);
}

static void drop_gpu_surface(vds_vaapi *h)
{
    if (h->gpu_surface != VA_INVALID_SURFACE) {
        vaDestroySurfaces(h->dpy, &h->gpu_surface, 1);
        h->gpu_surface = VA_INVALID_SURFACE;
    }
    h->gpu_generation = 0;
}

/*
 * Encodes the host GPU buffer `buf` (generation `generation`; imported again only when the
 * generation changes), flipped vertically when `flip` is set. Returns like vds_vaapi_encode;
 * -2 when the buffer cannot be used at all (import refused, flip unsupported).
 */
int32_t vds_vaapi_encode_dmabuf(vds_vaapi *h, const vds_vaapi_dmabuf *buf, uint64_t generation,
                                int32_t flip, int32_t force_idr, int64_t pts_ms,
                                const uint8_t **out)
{
    AVFrame *nv12 = NULL;
    int32_t size = -1;

    *out = NULL;
    if (buf->width != (uint32_t)h->width || buf->height != (uint32_t)h->height ||
        (flip && !h->can_flip)) {
        return -2;
    }
    if (h->gpu_surface == VA_INVALID_SURFACE || h->gpu_generation != generation) {
        drop_gpu_surface(h);
        if (import_dmabuf(h->dpy, buf, &h->gpu_surface) != VA_STATUS_SUCCESS) {
            h->gpu_surface = VA_INVALID_SURFACE;
            return -2;
        }
        h->gpu_generation = generation;
    }
    nv12 = av_frame_alloc();
    if (!nv12 || av_hwframe_get_buffer(h->nv12_frames, nv12, 0) < 0) {
        goto done;
    }
    if (vpp_convert(h, h->gpu_surface, (VASurfaceID)(uintptr_t)nv12->data[3], flip) < 0) {
        goto done;
    }
    size = encode_nv12(h, nv12, force_idr, pts_ms, out);

done:
    av_frame_free(&nv12);
    return size;
}

/* Hands the NV12 surface to the encoder and collects what it produced. */
static int32_t encode_nv12(vds_vaapi *h, AVFrame *nv12, int32_t force_idr, int64_t pts_ms,
                           const uint8_t **out)
{
    int32_t size = -1;
    int ret;

    *out = NULL;
    nv12->pts = pts_ms;
    nv12->pict_type = force_idr ? AV_PICTURE_TYPE_I : AV_PICTURE_TYPE_NONE;
    if (force_idr) {
        nv12->flags |= AV_FRAME_FLAG_KEY;
    }
    nv12->color_range = AVCOL_RANGE_JPEG;
    nv12->color_primaries = AVCOL_PRI_BT709;
    nv12->color_trc = AVCOL_TRC_BT709;
    nv12->colorspace = AVCOL_SPC_BT709;
    ret = avcodec_send_frame(h->enc, nv12);
    if (ret < 0) {
        return -1;
    }

    size = 0;
    for (;;) {
        ret = avcodec_receive_packet(h->enc, h->pkt);
        if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) {
            break;
        }
        if (ret < 0) {
            size = -1;
            break;
        }
        if ((size_t)size + (size_t)h->pkt->size > h->out_cap) {
            size_t cap = ((size_t)size + (size_t)h->pkt->size) * 2;
            uint8_t *grown = realloc(h->out, cap);

            if (!grown) {
                av_packet_unref(h->pkt);
                size = -1;
                break;
            }
            h->out = grown;
            h->out_cap = cap;
        }
        memcpy(h->out + size, h->pkt->data, (size_t)h->pkt->size);
        size += h->pkt->size;
        av_packet_unref(h->pkt);
    }
    if (size > 0) {
        *out = h->out;
    }
    return size;
}

int32_t vds_vaapi_set_rate(vds_vaapi *h, int32_t bitrate_kbps)
{
    /* h264_vaapi reads the rate at the next frame; out-of-range values are clamped by it. */
    return apply_rate(h, bitrate_kbps);
}

void vds_vaapi_close(vds_vaapi *h)
{
    if (!h) {
        return;
    }
    if (h->dpy) {
        drop_gpu_surface(h);
    }
    avcodec_free_context(&h->enc);
    av_packet_free(&h->pkt);
    av_frame_free(&h->sw);
    if (h->vpp_context != VA_INVALID_ID) {
        vaDestroyContext(h->dpy, h->vpp_context);
    }
    if (h->vpp_config != VA_INVALID_ID) {
        vaDestroyConfig(h->dpy, h->vpp_config);
    }
    av_buffer_unref(&h->nv12_frames);
    av_buffer_unref(&h->rgb_frames);
    av_buffer_unref(&h->device);
    free(h->out);
    free(h);
}

/* Checks, before any frame, that host GPU buffers can be encoded on this device. */
typedef struct vds_vaapi_probe {
    AVBufferRef *device;
    VADisplay dpy;
} vds_vaapi_probe;

vds_vaapi_probe *vds_vaapi_probe_open(const char *render_node, char *err, size_t err_len)
{
    vds_vaapi_probe *p = calloc(1, sizeof(*p));
    int ret;

    if (!p) {
        set_err(err, err_len, "out of memory", 0);
        return NULL;
    }
    ret = av_hwdevice_ctx_create(&p->device, AV_HWDEVICE_TYPE_VAAPI, render_node, NULL, 0);
    if (ret < 0) {
        set_err(err, err_len, "cannot open the VA-API device", ret);
        free(p);
        return NULL;
    }
    p->dpy = ((AVVAAPIDeviceContext *)((AVHWDeviceContext *)p->device->data)->hwctx)->display;
    return p;
}

/*
 * Imports `buf` once and checks the video processor can read it (and flip it when `flip` is
 * set). Returns 0, or -1 with the reason in `err`.
 */
int32_t vds_vaapi_probe_check(vds_vaapi_probe *p, const vds_vaapi_dmabuf *buf, int32_t flip,
                              char *err, size_t err_len)
{
    VASurfaceID surface = VA_INVALID_SURFACE;
    VAConfigID config = VA_INVALID_ID;
    VAContextID context = VA_INVALID_ID;
    VAProcPipelineCaps caps;
    VAStatus vs;
    int32_t rc = -1;

    if (!va_fourcc(buf->fourcc)) {
        snprintf(err, err_len, "unsupported format 0x%08x", buf->fourcc);
        return -1;
    }
    vs = import_dmabuf(p->dpy, buf, &surface);
    if (vs != VA_STATUS_SUCCESS) {
        snprintf(err, err_len, "VA-API cannot import the buffer: %s", vaErrorStr(vs));
        return -1;
    }
    vs = vaCreateConfig(p->dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &config);
    if (vs != VA_STATUS_SUCCESS) {
        snprintf(err, err_len, "VA video processing unavailable: %s", vaErrorStr(vs));
        goto out;
    }
    vs = vaCreateContext(p->dpy, config, buf->width, buf->height, VA_PROGRESSIVE, NULL, 0,
                         &context);
    if (vs != VA_STATUS_SUCCESS) {
        snprintf(err, err_len, "cannot create the VA video processor: %s", vaErrorStr(vs));
        goto out;
    }
    memset(&caps, 0, sizeof(caps));
    if (flip && (vaQueryVideoProcPipelineCaps(p->dpy, context, NULL, 0, &caps) !=
                     VA_STATUS_SUCCESS ||
                 !(caps.mirror_flags & VA_MIRROR_VERTICAL))) {
        snprintf(err, err_len, "the VA video processor cannot flip the picture");
        goto out;
    }
    rc = 0;

out:
    if (context != VA_INVALID_ID) {
        vaDestroyContext(p->dpy, context);
    }
    if (config != VA_INVALID_ID) {
        vaDestroyConfig(p->dpy, config);
    }
    vaDestroySurfaces(p->dpy, &surface, 1);
    return rc;
}

void vds_vaapi_probe_close(vds_vaapi_probe *p)
{
    if (!p) {
        return;
    }
    av_buffer_unref(&p->device);
    free(p);
}

/*
 * Tests only: a `width`x`height` XBGR8888 GPU buffer filled with `rgbx`, exported as a dmabuf
 * the way a host would hand it over. Returns the fd (the caller closes it) or -1.
 */
int32_t vds_vaapi_test_dmabuf(vds_vaapi_probe *p, uint32_t width, uint32_t height,
                              uint32_t rgbx, vds_vaapi_dmabuf *out)
{
    VASurfaceAttrib attr;
    VASurfaceID surface;
    VADRMPRIMESurfaceDescriptor desc;
    VAImage image;
    uint8_t *data = NULL;
    uint32_t y, x;
    int32_t fd = -1;

    memset(&attr, 0, sizeof(attr));
    attr.type = VASurfaceAttribPixelFormat;
    attr.flags = VA_SURFACE_ATTRIB_SETTABLE;
    attr.value.type = VAGenericValueTypeInteger;
    attr.value.value.i = VA_FOURCC_RGBX;
    if (vaCreateSurfaces(p->dpy, VA_RT_FORMAT_RGB32, width, height, &surface, 1, &attr, 1) !=
        VA_STATUS_SUCCESS) {
        return -1;
    }
    if (vaDeriveImage(p->dpy, surface, &image) == VA_STATUS_SUCCESS) {
        if (vaMapBuffer(p->dpy, image.buf, (void **)&data) == VA_STATUS_SUCCESS) {
            for (y = 0; y < height; y++) {
                uint32_t *row = (uint32_t *)(data + image.offsets[0] + y * image.pitches[0]);

                for (x = 0; x < width; x++) {
                    row[x] = rgbx;
                }
            }
            vaUnmapBuffer(p->dpy, image.buf);
        }
        vaDestroyImage(p->dpy, image.image_id);
    }
    vaSyncSurface(p->dpy, surface);
    if (vaExportSurfaceHandle(p->dpy, surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                              VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                              &desc) == VA_STATUS_SUCCESS) {
        fd = desc.objects[0].fd;
        out->fd = fd;
        out->width = width;
        out->height = height;
        out->stride = desc.layers[0].pitch[0];
        out->fourcc = desc.layers[0].drm_format;
        out->modifier = desc.objects[0].drm_format_modifier;
        for (x = 1; x < desc.num_objects; x++) {
            close(desc.objects[x].fd);
        }
    }
    /* The exported fd keeps the memory alive. */
    vaDestroySurfaces(p->dpy, &surface, 1);
    return fd;
}
