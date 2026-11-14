//! Unix: a dmabuf is a file descriptor.

use std::os::fd::{AsRawFd as _, OwnedFd};

/// A GPU buffer handle owned by libvds (a dmabuf file descriptor), closed on drop.
#[derive(Debug)]
pub struct BufferFd(OwnedFd);

impl BufferFd {
    /// Takes ownership of `fd`.
    pub fn new(fd: OwnedFd) -> Self {
        Self(fd)
    }

    /// The raw descriptor, valid while `self` lives.
    pub fn raw(&self) -> i32 {
        self.0.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::BufferFd;
    use crate::Core;
    use crate::config::ServerConfig;
    use crate::display::{ConsumedNotify, ConsumerId, Display, GpuBuffer};
    use crate::event::ClientEvent;
    use crate::geom::Rect;

    const C: ConsumerId = 1;

    #[derive(Debug, Default)]
    struct Count(AtomicU32);

    impl ConsumedNotify for Count {
        fn consumed(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn buffer(w: u32, h: u32) -> GpuBuffer {
        GpuBuffer {
            fd: BufferFd::new(File::open("/dev/null").unwrap().into()),
            width: w,
            height: h,
            stride: w * 4,
            fourcc: 0x3432_4258,
            modifier: u64::MAX >> 8,
            y0_top: false,
        }
    }

    #[test]
    fn gpu_frame_tells_the_host_once_read() {
        let d = Display::new();
        let h = d.add_head(0, 64, 32).unwrap();
        d.add_consumer(C);
        let n = Arc::new(Count::default());
        h.attach_gpu(buffer(64, 32), Some(n.clone())).unwrap();
        // Attaching damages the head, but the host has not reported damage: nobody waits.
        let frame = h.take_frame(C, |f, damage, _| {
            assert!(f.pixels.is_empty());
            assert_eq!(damage.rects(), &[Rect::new(0, 0, 64, 32)]);
            f.gpu.unwrap()
        });
        assert_eq!(frame.generation, 1);
        drop(frame);
        assert_eq!(n.0.load(Ordering::SeqCst), 0);

        h.damage(&[Rect::new(0, 0, 64, 32)]);
        let frame = h.take_frame(C, |f, _, _| f.gpu.unwrap());
        let copy = frame.clone();
        drop(frame);
        assert_eq!(n.0.load(Ordering::SeqCst), 0, "a clone still reads the buffer");
        drop(copy);
        assert_eq!(n.0.load(Ordering::SeqCst), 1);

        // with_frame does not take the damage, so it carries no ticket.
        h.damage(&[Rect::new(0, 0, 8, 8)]);
        drop(h.with_frame(|f| f.gpu.unwrap()));
        assert_eq!(n.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn letting_the_buffer_go_releases_a_waiting_host() {
        let d = Display::new();
        let h = d.add_head(0, 16, 16).unwrap();
        d.add_consumer(C);
        let n = Arc::new(Count::default());
        h.attach_gpu(buffer(16, 16), Some(n.clone())).unwrap();
        h.damage(&[Rect::new(0, 0, 16, 16)]);
        h.submit(&[0; 16 * 16 * 4], 64, &[Rect::new(0, 0, 16, 16)]).unwrap();
        assert_eq!(n.0.load(Ordering::SeqCst), 1);
        assert!(!h.has_gpu());

        h.attach_gpu(buffer(16, 16), Some(n.clone())).unwrap();
        h.damage(&[Rect::new(0, 0, 1, 1)]);
        h.attach_gpu(buffer(16, 16), Some(n.clone())).unwrap();
        assert_eq!(n.0.load(Ordering::SeqCst), 2);
        assert_eq!(h.take_frame(C, |f, _, _| f.gpu.unwrap().generation), 3);

        h.damage(&[Rect::new(0, 0, 1, 1)]);
        h.detach();
        assert_eq!(n.0.load(Ordering::SeqCst), 3);
        h.take_frame(C, |f, _, _| {
            assert!(f.gpu.is_none());
            assert!(f.pixels.iter().all(|b| *b == 0));
        });
    }

    #[test]
    fn gpu_buffer_must_match_the_head() {
        let d = Display::new();
        let h = d.add_head(0, 16, 16).unwrap();
        assert!(h.attach_gpu(buffer(32, 16), None).is_err());
        let mut b = buffer(16, 16);
        b.stride = 8;
        assert!(h.attach_gpu(b, None).is_err());
        assert!(!h.has_gpu());
    }

    #[test]
    fn cpu_demand_is_announced_on_first_and_last() {
        let core = Core::new(ServerConfig::default()).unwrap();
        let a = core.demand_cpu(2, "legacy");
        let b = core.demand_cpu(2, "encoder");
        assert!(core.cpu_demanded(2));
        drop(a);
        assert!(core.cpu_demanded(2));
        drop(b);
        assert!(!core.cpu_demanded(2));
        let mut events = Vec::new();
        core.drain_events(|ev| {
            if let ClientEvent::HeadInput { head, cpu, reason } = ev {
                events.push((head, cpu, reason));
            }
        });
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], (2, true, String::from("legacy")));
        assert!(!events[1].1);
    }
}
