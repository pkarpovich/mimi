use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub const DEFAULT_SLOTS: usize = 64;
pub const DEFAULT_FRAMES_PER_BLOCK: usize = 4096;

/// Block is one IOProc callback on its way to the writer, with the two tracks still apart.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub microphone: Vec<f32>,
    pub system: Vec<f32>,
    pub frames: usize,
    pub host_time: u64,
    pub generation: u64,
}

/// BlockRef is what the IOProc hands the ring, borrowing the frames it already holds.
#[derive(Debug, Clone, Copy)]
pub struct BlockRef<'a> {
    pub microphone: &'a [f32],
    pub system: &'a [f32],
    pub frames: usize,
    pub host_time: u64,
    pub generation: u64,
}

/// Drained is what one pass over the ring yielded and how many blocks it missed.
#[derive(Debug)]
pub struct Drained<'a> {
    pub blocks: &'a [Block],
    pub dropped: u64,
}

/// ring builds the fixed-capacity hand-off from the IOProc to the writer thread.
pub fn ring(slots: usize, frames_per_block: usize) -> (Producer, Consumer) {
    let slots = slots.max(1);
    let frames_per_block = frames_per_block.max(1);

    let mut cells = Vec::with_capacity(slots);
    for _ in 0..slots {
        cells.push(Slot::new(frames_per_block));
    }
    let shared = Arc::new(Shared {
        slots: cells,
        frames_per_block,
        write: AtomicUsize::new(0),
        read: AtomicUsize::new(0),
        dropped: AtomicU64::new(0),
    });

    let mut blocks = Vec::with_capacity(slots);
    for _ in 0..slots {
        blocks.push(Block {
            microphone: Vec::with_capacity(frames_per_block),
            system: Vec::with_capacity(frames_per_block),
            frames: 0,
            host_time: 0,
            generation: 0,
        });
    }
    let consumer = Consumer {
        shared: Arc::clone(&shared),
        blocks,
        filled: 0,
        dropped: 0,
    };
    (Producer(shared), consumer)
}

/// Producer is the write end the IOProc pushes each callback into.
#[derive(Clone)]
pub struct Producer(Arc<Shared>);

impl Producer {
    pub fn push(&self, block: BlockRef<'_>) {
        let Self(shared) = self;
        let BlockRef {
            microphone,
            system,
            frames,
            host_time,
            generation,
        } = block;

        if microphone.len() > shared.frames_per_block || system.len() > shared.frames_per_block {
            shared.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let write = shared.write.load(Ordering::Relaxed);
        let read = shared.read.load(Ordering::Acquire);
        if write - read == shared.slots.len() {
            shared.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let slot = &shared.slots[write % shared.slots.len()];
        store_samples(&slot.microphone, microphone);
        store_samples(&slot.system, system);
        slot.microphone_len
            .store(microphone.len(), Ordering::Relaxed);
        slot.system_len.store(system.len(), Ordering::Relaxed);
        slot.frames.store(frames, Ordering::Relaxed);
        slot.host_time.store(host_time, Ordering::Relaxed);
        slot.generation.store(generation, Ordering::Relaxed);
        shared.write.store(write + 1, Ordering::Release);
    }

    pub fn frames_per_block(&self) -> usize {
        let Self(shared) = self;
        shared.frames_per_block
    }
}

/// Consumer is the read end the writer thread drains.
pub struct Consumer {
    shared: Arc<Shared>,
    blocks: Vec<Block>,
    filled: usize,
    dropped: u64,
}

impl Consumer {
    pub fn frames_per_block(&self) -> usize {
        self.shared.frames_per_block
    }

    pub fn drain(&mut self) -> Drained<'_> {
        let write = self.shared.write.load(Ordering::Acquire);
        let read = self.shared.read.load(Ordering::Relaxed);

        self.filled = 0;
        let mut index = read;
        while index < write {
            let slot = &self.shared.slots[index % self.shared.slots.len()];
            let Block {
                microphone,
                system,
                frames,
                host_time,
                generation,
            } = &mut self.blocks[self.filled];
            load_samples(
                &slot.microphone,
                slot.microphone_len.load(Ordering::Relaxed),
                microphone,
            );
            load_samples(
                &slot.system,
                slot.system_len.load(Ordering::Relaxed),
                system,
            );
            *frames = slot.frames.load(Ordering::Relaxed);
            *host_time = slot.host_time.load(Ordering::Relaxed);
            *generation = slot.generation.load(Ordering::Relaxed);
            self.filled += 1;
            index += 1;
        }
        self.shared.read.store(write, Ordering::Release);

        let dropped = self.shared.dropped.load(Ordering::Relaxed);
        let since = dropped - self.dropped;
        self.dropped = dropped;
        Drained {
            blocks: &self.blocks[..self.filled],
            dropped: since,
        }
    }
}

struct Shared {
    slots: Vec<Slot>,
    frames_per_block: usize,
    write: AtomicUsize,
    read: AtomicUsize,
    dropped: AtomicU64,
}

struct Slot {
    microphone: Vec<AtomicU32>,
    system: Vec<AtomicU32>,
    microphone_len: AtomicUsize,
    system_len: AtomicUsize,
    frames: AtomicUsize,
    host_time: AtomicU64,
    generation: AtomicU64,
}

impl Slot {
    fn new(frames_per_block: usize) -> Self {
        let mut microphone = Vec::with_capacity(frames_per_block);
        let mut system = Vec::with_capacity(frames_per_block);
        for _ in 0..frames_per_block {
            microphone.push(AtomicU32::new(0));
            system.push(AtomicU32::new(0));
        }
        Self {
            microphone,
            system,
            microphone_len: AtomicUsize::new(0),
            system_len: AtomicUsize::new(0),
            frames: AtomicUsize::new(0),
            host_time: AtomicU64::new(0),
            generation: AtomicU64::new(0),
        }
    }
}

fn store_samples(slots: &[AtomicU32], samples: &[f32]) {
    for (index, sample) in samples.iter().enumerate() {
        slots[index].store(sample.to_bits(), Ordering::Relaxed);
    }
}

fn load_samples(slots: &[AtomicU32], len: usize, samples: &mut Vec<f32>) {
    samples.clear();
    for sample in &slots[..len] {
        samples.push(f32::from_bits(sample.load(Ordering::Relaxed)));
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    fn block(host_time: u64) -> BlockRef<'static> {
        BlockRef {
            microphone: &[0.25, 0.5],
            system: &[-1.0, 1.0],
            frames: 2,
            host_time,
            generation: 7,
        }
    }

    #[test]
    fn a_pushed_block_comes_back_field_for_field() {
        let (producer, mut consumer) = ring(4, 8);
        producer.push(block(1234));

        let Drained { blocks, dropped } = consumer.drain();
        assert_eq!(dropped, 0);
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0],
            Block {
                microphone: vec![0.25, 0.5],
                system: vec![-1.0, 1.0],
                frames: 2,
                host_time: 1234,
                generation: 7,
            }
        );
    }

    #[test]
    fn an_absent_microphone_track_stays_absent() {
        let (producer, mut consumer) = ring(4, 8);
        producer.push(BlockRef {
            microphone: &[],
            system: &[0.5, 0.5, 0.5],
            frames: 3,
            host_time: 9,
            generation: 0,
        });

        let Drained { blocks, dropped } = consumer.drain();
        assert_eq!(dropped, 0);
        assert!(blocks[0].microphone.is_empty());
        assert_eq!(blocks[0].system, vec![0.5, 0.5, 0.5]);
    }

    #[test]
    fn the_ring_wraps_around_its_slots() {
        let (producer, mut consumer) = ring(2, 8);
        let mut seen = Vec::new();
        for round in 0..5 {
            producer.push(block(round));
            let Drained { blocks, dropped } = consumer.drain();
            assert_eq!(dropped, 0);
            assert_eq!(blocks.len(), 1);
            seen.push(blocks[0].host_time);
        }
        assert_eq!(seen, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_producer_that_outran_the_consumer_reports_the_drops() {
        let (producer, mut consumer) = ring(2, 8);
        for round in 0..4 {
            producer.push(block(round));
        }

        let Drained { blocks, dropped } = consumer.drain();
        assert_eq!(blocks.len(), 2);
        assert_eq!(dropped, 2);
        assert_eq!(blocks[0].host_time, 0);
        assert_eq!(blocks[1].host_time, 1);

        let Drained { blocks, dropped } = consumer.drain();
        assert!(blocks.is_empty());
        assert_eq!(dropped, 0, "drops are reported once, not on every drain");
    }

    #[test]
    fn a_full_ring_never_blocks_the_producer() {
        let (producer, mut consumer) = ring(2, 8);
        for round in 0..1000 {
            producer.push(block(round));
        }

        let Drained { blocks, dropped } = consumer.drain();
        assert_eq!(blocks.len(), 2);
        assert_eq!(dropped, 998);
    }

    #[test]
    fn draining_an_empty_ring_yields_nothing() {
        let (_producer, mut consumer) = ring(4, 8);
        let Drained { blocks, dropped } = consumer.drain();
        assert!(blocks.is_empty());
        assert_eq!(dropped, 0);
    }

    #[test]
    fn a_block_longer_than_a_slot_is_dropped_rather_than_truncated() {
        let (producer, mut consumer) = ring(4, 2);
        producer.push(BlockRef {
            microphone: &[],
            system: &[0.1, 0.2, 0.3],
            frames: 3,
            host_time: 1,
            generation: 0,
        });

        let Drained { blocks, dropped } = consumer.drain();
        assert!(blocks.is_empty());
        assert_eq!(dropped, 1);
    }

    #[test]
    fn the_producer_reports_the_slot_capacity_the_io_proc_must_stay_within() {
        let (producer, consumer) = ring(4, 512);
        assert_eq!(producer.frames_per_block(), 512);
        assert_eq!(consumer.frames_per_block(), 512);
    }

    #[test]
    fn a_block_crossing_the_threads_arrives_whole_and_in_order() {
        const BLOCKS: u64 = 20_000;
        let (producer, mut consumer) = ring(64, 32);
        let pushing = thread::spawn(move || {
            for round in 0..BLOCKS {
                let samples = vec![round as f32; 32];
                producer.push(BlockRef {
                    microphone: &samples,
                    system: &samples,
                    frames: 32,
                    host_time: round,
                    generation: round,
                });
            }
        });

        let mut seen = 0;
        let mut dropped = 0;
        let mut previous = None;
        while seen + dropped < BLOCKS {
            let Drained {
                blocks,
                dropped: missed,
            } = consumer.drain();
            dropped += missed;
            for block in blocks {
                let Block {
                    microphone,
                    system,
                    frames,
                    host_time,
                    generation,
                } = block;
                assert_eq!(*frames, 32);
                assert_eq!(
                    *generation, *host_time,
                    "a block was torn between two pushes"
                );
                assert_eq!(
                    microphone,
                    &vec![*host_time as f32; 32],
                    "the microphone track carries samples from another block"
                );
                assert_eq!(
                    system, microphone,
                    "the two tracks came from different blocks"
                );
                let Some(before) = previous else {
                    previous = Some(*host_time);
                    seen += 1;
                    continue;
                };
                assert!(
                    *host_time > before,
                    "blocks reached the writer out of order: {host_time} after {before}"
                );
                previous = Some(*host_time);
                seen += 1;
            }
        }
        pushing.join().expect("the producer thread");
        assert_eq!(seen + dropped, BLOCKS);
    }

    #[test]
    fn a_drained_ring_takes_new_blocks_again() {
        let (producer, mut consumer) = ring(2, 8);
        for round in 0..3 {
            producer.push(block(round));
        }
        let Drained { blocks, dropped } = consumer.drain();
        assert_eq!(blocks.len(), 2);
        assert_eq!(dropped, 1);

        producer.push(block(99));
        let Drained { blocks, dropped } = consumer.drain();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].host_time, 99);
        assert_eq!(dropped, 0);
    }
}
