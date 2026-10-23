/*
 * H.264 encoding on the GPU through VA-API.
 *
 * BGRX pixels are uploaded into an RGB VA surface, converted to NV12 by the VA video
 * processor (full-range BT.709, the matrix RDP clients use for AVC420) and encoded by
 * FFmpeg's h264_vaapi. The CPU only copies the pixels into the upload surface.
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
#include <va/va.h>
#include <va/va_vpp.h>

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

/* Converts the RGB surface `src` into the NV12 surface `dst` on the GPU. */
static int vpp_convert(vds_vaapi *h, VASurfaceID src, VASurfaceID dst)
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
    int ret;

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
                    (VASurfaceID)(uintptr_t)nv12->data[3]) < 0) {
        goto done;
    }

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
        goto done;
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

done:
    if (h->sw) {
        h->sw->data[0] = NULL;
    }
    av_frame_free(&rgb);
    av_frame_free(&nv12);
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
