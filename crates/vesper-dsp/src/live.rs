//! A [`Chain`] that can be changed while it runs: parameter changes go
//! through its [`ChainControl`], changes of structure build a new chain on
//! the controlling thread and hand it to the audio thread through a
//! lock-free single-slot exchange. The audio thread hands the old chain
//! back the same way, so building and freeing both stay off the audio path.

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::{Chain, ChainControl, FilterSpec};

/// The two slots between [`LiveControl`] and [`LiveChain`].
#[derive(Default)]
struct Exchange {
    /// A new chain for the audio thread; it takes it with a swap.
    incoming: AtomicPtr<Chain>,
    /// The chain the audio thread replaced, for the control to free. The
    /// audio thread only takes a new chain while this is empty.
    trash: AtomicPtr<Chain>,
}

impl Exchange {
    fn take(slot: &AtomicPtr<Chain>) -> Option<Box<Chain>> {
        let p = slot.swap(ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: non-null pointers in the slots come from `Box::into_raw`,
        // and the swap gives this caller sole ownership.
        (!p.is_null()).then(|| unsafe { Box::from_raw(p) })
    }
}

impl Drop for Exchange {
    fn drop(&mut self) {
        drop(Self::take(&self.incoming));
        drop(Self::take(&self.trash));
    }
}

/// A live chain and its control, for `channels` interleaved channels.
pub fn live(specs: &[FilterSpec], channels: usize) -> (LiveChain, LiveControl) {
    let (chain, control) = Chain::new(specs, channels);
    let exchange = Arc::new(Exchange::default());
    let latency = chain.latency();
    (
        LiveChain {
            chain: Box::new(chain),
            exchange: exchange.clone(),
            latency,
            warm: false,
        },
        LiveControl {
            control,
            exchange,
            channels,
        },
    )
}

/// The audio thread's side of a live chain.
pub struct LiveChain {
    chain: Box<Chain>,
    exchange: Arc<Exchange>,
    latency: usize,
    warm: bool,
}

impl LiveChain {
    /// Takes a chain handed over since the last block, then runs the chain
    /// on `buf` (see [`Chain::process`]). The first call warms the thread up
    /// (see [`Chain::warm_up`]) and allocates; later calls do not allocate,
    /// lock or free, a handover included.
    pub fn process(&mut self, buf: &mut [f32]) {
        if !self.warm {
            self.warm = true;
            // A later chain may bring noise suppression: warm its FFT now.
            let channels = self.chain.channels;
            let spec = FilterSpec::default_for("noise_suppression").expect("known filter");
            Chain::new(&[spec], channels).0.warm_up();
            self.chain.warm_up();
        }
        let ex = &self.exchange;
        if ex.trash.load(Ordering::Acquire).is_null()
            && let Some(new) = Exchange::take(&ex.incoming)
        {
            self.latency = new.latency();
            let old = std::mem::replace(&mut self.chain, new);
            ex.trash.store(Box::into_raw(old), Ordering::Release);
        }
        self.chain.process(buf);
    }

    /// Delay of the running chain, in frames: it changes when a new chain
    /// is taken.
    pub fn latency(&self) -> usize {
        self.latency
    }
}

/// The controlling side of a live chain.
pub struct LiveControl {
    control: ChainControl,
    exchange: Arc<Exchange>,
    channels: usize,
}

impl LiveControl {
    /// Filters of the newest chain.
    pub fn specs(&self) -> &[FilterSpec] {
        self.control.specs()
    }

    /// Makes the chain run `specs`: in place, gliding, when only parameters
    /// change; otherwise through a new chain, built here and taken by the
    /// audio thread at its next block. Returns whether a new chain was made.
    pub fn set(&mut self, specs: &[FilterSpec]) -> bool {
        self.collect();
        let same = specs.len() == self.control.specs().len()
            && specs
                .iter()
                .enumerate()
                .all(|(i, s)| self.control.set(i, *s));
        if same {
            return false;
        }
        let (chain, control) = Chain::new(specs, self.channels);
        self.control = control;
        let new = Box::into_raw(Box::new(chain));
        // A chain the audio thread never took is replaced and freed here.
        let stale = self.exchange.incoming.swap(new, Ordering::AcqRel);
        if !stale.is_null() {
            // SAFETY: it came from `Box::into_raw` and the swap took it back.
            drop(unsafe { Box::from_raw(stale) });
        }
        // The audio thread may have filled the trash while the chain was
        // built: empty it, or the new chain would wait for the next change.
        self.collect();
        true
    }

    /// Frees the chain the audio thread replaced, if any.
    pub fn collect(&self) {
        drop(Exchange::take(&self.exchange.trash));
    }

    /// Gain reduction of filter `index` of the newest chain, dB (see
    /// [`ChainControl::reduction_db`]).
    pub fn reduction_db(&self, index: usize) -> f32 {
        self.control.reduction_db(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(text: &str) -> FilterSpec {
        text.parse().unwrap()
    }

    #[test]
    fn parameter_changes_stay_in_place() {
        let (mut chain, mut control) = live(&[spec("gain:db=0")], 2);
        assert!(!control.set(&[spec("gain:db=-6")]));
        let mut buf = vec![1.0; 9_600 * 2];
        chain.process(&mut buf);
        assert!((buf.last().unwrap() - 0.501).abs() < 1e-3);
        assert_eq!(control.specs(), [spec("gain:db=-6")]);
    }

    #[test]
    fn structure_changes_hand_over_a_new_chain() {
        let (mut chain, mut control) = live(&[spec("gain:db=-6")], 2);
        let mut buf = vec![1.0; 960 * 2];
        chain.process(&mut buf);
        assert_eq!(chain.latency(), 0);
        // Two changes before the audio thread looks: only the last runs.
        assert!(control.set(&[spec("limiter:lookahead_ms=5")]));
        assert!(control.set(&[spec("gain:db=-12"), spec("limiter:lookahead_ms=2")]));
        chain.process(&mut buf);
        assert_eq!(chain.latency(), 96 + 3);
        // The old chain waits for the control to free it.
        assert!(!control.exchange.trash.load(Ordering::Relaxed).is_null());
        assert!(control.set(&[spec("high_pass")]));
        assert!(control.exchange.trash.load(Ordering::Relaxed).is_null());
        chain.process(&mut buf);
        assert_eq!(chain.latency(), 0);
        // While the trash is full, the audio thread leaves the new chain.
        let (other, _) = Chain::new(&[spec("limiter")], 2);
        let other = Box::into_raw(Box::new(other));
        control.exchange.incoming.store(other, Ordering::Release);
        chain.process(&mut buf);
        assert_eq!(chain.latency(), 0);
        control.collect();
        chain.process(&mut buf);
        assert_eq!(chain.latency(), 99);
    }

    #[test]
    fn reduction_is_published() {
        let (mut chain, control) = live(&[spec("limiter:ceiling_db=-6")], 2);
        let mut buf = vec![1.0; 4_800 * 2];
        chain.process(&mut buf);
        assert!(
            (control.reduction_db(0) - 6.0).abs() < 0.3,
            "{}",
            control.reduction_db(0)
        );
        assert_eq!(control.reduction_db(1), 0.0);
    }
}
