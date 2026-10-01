//! Rewind: a rolling history of recent save states to step back through.
//!
//! A snapshot is taken every few frames. Storing each whole (~24 KiB) would
//! cost tens of megabytes for half a minute, but most of a state doesn't
//! change between snapshots: the newest is kept whole, and each older one as
//! a "reverse delta", the bytes that differ from the snapshot after it
//! (XOR, with runs of unchanged bytes skipped). Stepping back applies the
//! newest delta; forgetting the oldest snapshot just drops the oldest delta.

use crate::GameBoy;
use std::collections::VecDeque;

pub struct Rewind {
    /// Frames between snapshots.
    every: u32,
    /// Frames since the last snapshot.
    frames: u32,
    max_snapshots: usize,
    /// Memory the deltas may use, in bytes; the oldest go first.
    max_bytes: usize,
    newest: Option<Vec<u8>>,
    /// Oldest first: `deltas[i]` turns snapshot i+1 back into snapshot i
    /// (the last turns `newest` into the one before it).
    deltas: VecDeque<Vec<u8>>,
    delta_bytes: usize,
}

impl Rewind {
    /// Snapshots every `every` frames, keeping at most `max_snapshots` of
    /// them and `max_bytes` of history.
    pub fn new(every: u32, max_snapshots: usize, max_bytes: usize) -> Self {
        Self {
            every: every.max(1),
            frames: 0,
            max_snapshots: max_snapshots.max(1),
            max_bytes,
            newest: None,
            deltas: VecDeque::new(),
            delta_bytes: 0,
        }
    }

    /// Call once per frame run: takes a snapshot when one is due.
    pub fn record(&mut self, gb: &GameBoy) {
        self.frames += 1;
        if self.frames >= self.every {
            self.frames = 0;
            self.push(gb.save_state());
        }
    }

    /// Loads the newest snapshot into `gb` and forgets it, so calling again
    /// goes further back. False when there's nothing left to go back to.
    pub fn step_back(&mut self, gb: &mut GameBoy) -> bool {
        let Some(state) = self.pop() else {
            return false;
        };
        self.frames = 0; // the next snapshot is a full interval from here
        if gb.load_state(&state).is_err() {
            self.clear(); // can't happen with our own states; don't keep trying
            return false;
        }
        true
    }

    /// How many frames back the history reaches.
    pub fn frames_available(&self) -> u32 {
        self.len() as u32 * self.every
    }

    /// Snapshots held.
    pub fn len(&self) -> usize {
        usize::from(self.newest.is_some()) + self.deltas.len()
    }

    pub fn is_empty(&self) -> bool {
        self.newest.is_none()
    }

    /// Bytes the history uses.
    pub fn bytes(&self) -> usize {
        self.newest.as_ref().map_or(0, Vec::len) + self.delta_bytes
    }

    pub fn clear(&mut self) {
        self.newest = None;
        self.deltas.clear();
        self.delta_bytes = 0;
        self.frames = 0;
    }

    fn push(&mut self, state: Vec<u8>) {
        if let Some(prev) = self.newest.take() {
            if prev.len() == state.len() {
                let delta = delta(&state, &prev);
                self.delta_bytes += delta.len();
                self.deltas.push_back(delta);
            } else {
                self.deltas.clear(); // can't be: one game's states are one size
                self.delta_bytes = 0;
            }
        }
        self.newest = Some(state);
        while self.len() > self.max_snapshots
            || (self.delta_bytes > self.max_bytes && !self.deltas.is_empty())
        {
            if let Some(old) = self.deltas.pop_front() {
                self.delta_bytes -= old.len();
            }
        }
    }

    fn pop(&mut self) -> Option<Vec<u8>> {
        let current = self.newest.take()?;
        if let Some(d) = self.deltas.pop_back() {
            self.delta_bytes -= d.len();
            let mut older = current.clone();
            if apply(&mut older, &d).is_ok() {
                self.newest = Some(older);
            } else {
                self.clear();
            }
        }
        Some(current)
    }
}

/// Unchanged bytes in a row that end a run of changed ones: below this,
/// starting a new run costs more than carrying the zeros along.
const GAP: usize = 4;

/// What turns `from` into `to` (same length): runs of
/// (unchanged count, changed count, the changed bytes XORed), counts as LEB128.
fn delta(from: &[u8], to: &[u8]) -> Vec<u8> {
    let x: Vec<u8> = from.iter().zip(to).map(|(a, b)| a ^ b).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < x.len() {
        let start = i;
        while i < x.len() && x[i] == 0 {
            i += 1;
        }
        if i == x.len() {
            break; // nothing changed to the end
        }
        let skip = i - start;
        let run_start = i;
        // Extend the run until GAP unchanged bytes in a row (or the end).
        while i < x.len() && !x[i..(i + GAP).min(x.len())].iter().all(|&b| b == 0) {
            i += 1;
        }
        leb128(&mut out, skip);
        leb128(&mut out, i - run_start);
        out.extend_from_slice(&x[run_start..i]);
    }
    out
}

/// Applies a `delta` to `target` in place. Err if it doesn't fit.
fn apply(target: &mut [u8], delta: &[u8]) -> Result<(), ()> {
    let mut pos = 0;
    let mut d = delta;
    while !d.is_empty() {
        let skip = read_leb128(&mut d)?;
        let len = read_leb128(&mut d)?;
        pos += skip;
        let (bytes, rest) = (d.get(..len).ok_or(())?, &d[len..]);
        let dest = target.get_mut(pos..pos + len).ok_or(())?;
        for (t, b) in dest.iter_mut().zip(bytes) {
            *t ^= b;
        }
        pos += len;
        d = rest;
    }
    Ok(())
}

fn leb128(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let byte = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_leb128(d: &mut &[u8]) -> Result<usize, ()> {
    let mut n = 0usize;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = d.split_first().ok_or(())?;
        *d = rest;
        n |= usize::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(n);
        }
    }
    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes.
    fn noise(len: usize, seed: u32) -> Vec<u8> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 24) as u8
            })
            .collect()
    }

    fn round_trip(from: &[u8], to: &[u8]) -> Vec<u8> {
        let d = delta(from, to);
        let mut out = from.to_vec();
        apply(&mut out, &d).unwrap();
        assert_eq!(out, to);
        d
    }

    #[test]
    fn deltas_turn_one_buffer_into_another() {
        let a = noise(30_000, 1);
        assert!(round_trip(&a, &a).is_empty(), "no change, no delta");
        let mut b = a.clone();
        b[0] ^= 1;
        b[100] ^= 0xFF;
        b[101] ^= 0x10;
        b[29_999] ^= 0x80;
        let d = round_trip(&a, &b);
        assert!(d.len() < 20, "a few changes, a few bytes: {}", d.len());
        let c = noise(30_000, 2);
        round_trip(&a, &c); // everything different
        round_trip(&[], &[]);
    }

    #[test]
    fn short_gaps_stay_inside_a_run() {
        let a = vec![0u8; 64];
        let mut b = a.clone();
        b[10] = 1;
        b[12] = 1; // 1 unchanged byte between: one run
        b[30] = 1; // 17 unchanged before this: a new run
        let d = round_trip(&a, &b);
        // (skip 10, 3 bytes) + (skip 17, 1 byte): 2 + 3 + 2 + 1
        assert_eq!(d.len(), 8);
    }

    #[test]
    fn long_skips_use_multi_byte_counts() {
        let a = vec![0u8; 100_000];
        let mut b = a.clone();
        b[99_999] = 7;
        let d = round_trip(&a, &b);
        assert_eq!(d.len(), 3 + 1 + 1, "99,999 takes 3 LEB128 bytes");
    }

    #[test]
    fn a_delta_that_does_not_fit_is_refused() {
        let a = vec![0u8; 8];
        let mut b = a.clone();
        b[7] = 1;
        let d = delta(&a, &b);
        let mut short = vec![0u8; 4];
        assert!(apply(&mut short, &d).is_err());
        assert!(
            apply(&mut [0u8; 8], &d[..d.len() - 1]).is_err(),
            "cut short"
        );
        assert!(apply(&mut [0u8; 8], &[0x80, 0x80, 0x80, 0x80, 0x80, 0x01]).is_err());
    }

    #[test]
    fn snapshots_come_back_newest_first() {
        let mut r = Rewind::new(1, 100, usize::MAX);
        let states: Vec<Vec<u8>> = (0..10)
            .map(|i| {
                let mut s = noise(1000, 7);
                s[i * 50] ^= 0xAA; // each differs a little from the last
                s
            })
            .collect();
        for s in &states {
            r.push(s.clone());
        }
        assert_eq!(r.len(), 10);
        for s in states.iter().rev() {
            assert_eq!(r.pop().as_ref(), Some(s));
        }
        assert_eq!(r.pop(), None);
        assert!(r.is_empty());
    }

    #[test]
    fn the_oldest_go_when_full() {
        let mut r = Rewind::new(1, 3, usize::MAX);
        for i in 0..5u8 {
            r.push(vec![i; 16]);
        }
        assert_eq!(r.len(), 3);
        let order: Vec<u8> = std::iter::from_fn(|| r.pop()).map(|s| s[0]).collect();
        assert_eq!(order, [4, 3, 2]);

        // And by memory: each delta here is 2 + 16 bytes.
        let mut r = Rewind::new(1, 100, 40);
        for i in 0..5u8 {
            r.push(vec![i; 16]);
        }
        assert!(r.bytes() <= 16 + 40, "{}", r.bytes());
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn snapshots_are_taken_every_few_frames() {
        let gb = GameBoy::new(crate::cartridge::tests::rom_with_program(&[0x18, 0xFE])).unwrap();
        let mut r = Rewind::new(3, 100, usize::MAX);
        for _ in 0..8 {
            r.record(&gb);
        }
        assert_eq!(r.len(), 2, "after frames 3 and 6");
        assert_eq!(r.frames_available(), 6);
    }
}
