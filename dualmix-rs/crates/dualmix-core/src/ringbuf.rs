//! Wait-free, lock-free single-producer single-consumer (SPSC) audio ring buffer.
//! Cache-line aligned head and tail atomics eliminate false sharing across cores.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

#[repr(align(64))]
struct CachePadded(AtomicUsize);

/// Fixed-capacity wait-free SPSC sample ring buffer (power-of-two capacity).
pub struct SpscRing {
    mask: usize,
    buf: Box<[UnsafeCell<f32>]>,
    head: CachePadded, // written by consumer
    tail: CachePadded, // written by producer
}

unsafe impl Send for SpscRing {}
unsafe impl Sync for SpscRing {}

impl SpscRing {
    pub fn with_capacity(min_samples: usize) -> Self {
        let cap = min_samples.max(64).next_power_of_two();
        let buf = (0..cap).map(|_| UnsafeCell::new(0.0)).collect::<Vec<_>>().into_boxed_slice();
        Self {
            mask: cap - 1,
            buf,
            head: CachePadded(AtomicUsize::new(0)),
            tail: CachePadded(AtomicUsize::new(0)),
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    #[inline]
    pub fn available_read(&self) -> usize {
        let tail = self.tail.0.load(Ordering::Acquire);
        let head = self.head.0.load(Ordering::Relaxed);
        tail.wrapping_sub(head)
    }

    #[inline]
    pub fn available_write(&self) -> usize {
        let head = self.head.0.load(Ordering::Acquire);
        let tail = self.tail.0.load(Ordering::Relaxed);
        self.capacity() - tail.wrapping_sub(head)
    }

    /// Pushes as many samples as fit without blocking; returns count written.
    pub fn push(&self, src: &[f32]) -> usize {
        let head = self.head.0.load(Ordering::Acquire);
        let tail = self.tail.0.load(Ordering::Relaxed);
        let free = self.capacity() - tail.wrapping_sub(head);
        let n = src.len().min(free);
        for (i, &val) in src[..n].iter().enumerate() {
            let idx = (tail + i) & self.mask;
            unsafe { *self.buf[idx].get() = val; }
        }
        self.tail.0.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    /// Pops up to `dst.len()` samples; returns count read.
    pub fn pop(&self, dst: &mut [f32]) -> usize {
        let tail = self.tail.0.load(Ordering::Acquire);
        let head = self.head.0.load(Ordering::Relaxed);
        let avail = tail.wrapping_sub(head);
        let n = dst.len().min(avail);
        for (i, slot) in dst[..n].iter_mut().enumerate() {
            let idx = (head + i) & self.mask;
            *slot = unsafe { *self.buf[idx].get() };
        }
        self.head.0.store(head.wrapping_add(n), Ordering::Release);
        n
    }

    pub fn clear(&self) {
        let tail = self.tail.0.load(Ordering::Acquire);
        self.head.0.store(tail, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraparound_fifo_integrity() {
        let ring = SpscRing::with_capacity(64);
        let a: Vec<f32> = (0..50).map(|i| i as f32).collect();
        assert_eq!(ring.push(&a), 50);
        let mut out = [0.0f32; 30];
        assert_eq!(ring.pop(&mut out), 30);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[29], 29.0);

        let b: Vec<f32> = (100..140).map(|i| i as f32).collect();
        assert_eq!(ring.push(&b), 40);
        let mut rest = [0.0f32; 60];
        assert_eq!(ring.pop(&mut rest), 60);
        assert_eq!(rest[0], 30.0);
        assert_eq!(rest[19], 49.0);
        assert_eq!(rest[20], 100.0);
        assert_eq!(rest[59], 139.0);
    }
}
