//! Filter descriptions and their text form (`name:key=value,...`).

use std::fmt;
use std::str::FromStr;

use crate::Filter;
use crate::biquad::{Gain, HighPass};
use crate::denoise::Denoise;
use crate::dynamics::{Compressor, DeEsser, Expander};
use crate::limiter::Limiter;

/// Most parameters a filter has.
pub const MAX_PARAMS: usize = 6;

/// Declares [`FilterSpec`] with each parameter's default and valid range.
macro_rules! specs {
    ($(
        $(#[doc = $doc:literal])*
        $variant:ident = $name:literal {
            $( $(#[doc = $fdoc:literal])* $field:ident: $default:literal in $min:literal..=$max:literal, )+
        }
    )+) => {
        /// Parameters of a built-in filter. Defaults suit a voice.
        #[derive(Debug, Clone, Copy, PartialEq)]
        pub enum FilterSpec {
            $( $(#[doc = $doc])* $variant { $( $(#[doc = $fdoc])* $field: f32, )+ }, )+
        }

        impl FilterSpec {
            /// Every filter, with its default parameters.
            pub const DEFAULTS: &[FilterSpec] = &[$( FilterSpec::$variant { $( $field: $default, )+ }, )+];

            /// Name in the text form.
            pub fn name(&self) -> &'static str {
                match self { $( Self::$variant { .. } => $name, )+ }
            }

            /// Parameter names, in [`Filter::set_param`] order.
            pub fn param_names(&self) -> &'static [&'static str] {
                match self { $( Self::$variant { .. } => &[$( stringify!($field), )+], )+ }
            }

            /// Valid `(min, max)` of each parameter, in [`Self::param_names`] order.
            pub fn ranges(&self) -> &'static [(f32, f32)] {
                match self { $( Self::$variant { .. } => &[$( ($min, $max), )+], )+ }
            }

            /// Parameter values in [`Self::param_names`] order, then zeros.
            pub fn values(&self) -> [f32; MAX_PARAMS] {
                let mut out = [0.0; MAX_PARAMS];
                match *self {
                    $( Self::$variant { $( $field, )+ } => {
                        let v = [$( $field, )+];
                        out[..v.len()].copy_from_slice(&v);
                    } )+
                }
                out
            }

            fn value_mut(&mut self, i: usize) -> &mut f32 {
                let value = match self {
                    $( Self::$variant { $( $field, )+ } => [$( $field, )+].into_iter().nth(i), )+
                };
                value.expect("parameter index")
            }
        }
    };
}

specs! {
    /// RNNoise on the mix of the channels, 20 ms latency.
    NoiseSuppression = "noise_suppression" {
        /// Most the noise is lowered: the dry signal is mixed back in at
        /// this level. 100 is fully wet.
        max_attenuation_db: 30.0 in 0.0..=100.0,
    }
    /// Downward expander (soft noise gate).
    Expander = "expander" {
        threshold_db: -50.0 in -100.0..=0.0,
        /// dB of attenuation per dB below the threshold.
        ratio: 4.0 in 1.0..=20.0,
        /// Opening time.
        attack_ms: 2.0 in 0.1..=200.0,
        /// Closing time.
        release_ms: 150.0 in 1.0..=2000.0,
        /// Most attenuation.
        range_db: 24.0 in 0.0..=100.0,
    }
    /// Feed-forward compressor with a soft knee.
    Compressor = "compressor" {
        threshold_db: -20.0 in -60.0..=0.0,
        ratio: 3.0 in 1.0..=20.0,
        attack_ms: 5.0 in 0.1..=200.0,
        release_ms: 80.0 in 1.0..=2000.0,
        knee_db: 6.0 in 0.0..=24.0,
        makeup_db: 4.0 in -24.0..=24.0,
    }
    /// Split-band de-esser: compresses only the sibilance band.
    DeEsser = "de_esser" {
        /// Center of the sibilance band.
        freq_hz: 6500.0 in 2000.0..=12000.0,
        threshold_db: -30.0 in -60.0..=0.0,
        /// Most attenuation of the band.
        range_db: 8.0 in 0.0..=24.0,
    }
    /// Lookahead peak limiter, with intersample peak detection.
    Limiter = "limiter" {
        ceiling_db: -1.0 in -24.0..=0.0,
        release_ms: 60.0 in 1.0..=1000.0,
        /// Changing it changes the latency: it needs a new chain.
        lookahead_ms: 2.0 in 0.5..=10.0,
    }
    /// 12 dB/octave Butterworth high-pass.
    HighPass = "high_pass" {
        /// -3 dB point.
        freq_hz: 80.0 in 10.0..=1000.0,
    }
    Gain = "gain" {
        db: 0.0 in -60.0..=24.0,
    }
}

impl FilterSpec {
    /// The filter called `name`, with its default parameters.
    pub fn default_for(name: &str) -> Option<Self> {
        Self::DEFAULTS.iter().find(|s| s.name() == name).copied()
    }

    /// A voice chain, in order: high-pass, noise suppression, expander,
    /// compressor, de-esser, limiter.
    pub fn voice_chain() -> Vec<Self> {
        [
            "high_pass",
            "noise_suppression",
            "expander",
            "compressor",
            "de_esser",
            "limiter",
        ]
        .into_iter()
        .filter_map(Self::default_for)
        .collect()
    }

    /// The same filter with every value in its range (NaN: the default).
    pub fn clamped(mut self) -> Self {
        let defaults = Self::default_for(self.name())
            .expect("known filter")
            .values();
        for (i, &(lo, hi)) in self.ranges().iter().enumerate() {
            let v = self.value_mut(i);
            *v = if v.is_nan() {
                defaults[i]
            } else {
                v.clamp(lo, hi)
            };
        }
        self
    }

    /// The same filter with parameter `i` set to `value` (clamped; NaN:
    /// the default). An index past the parameters changes nothing.
    pub fn with(mut self, i: usize, value: f32) -> Self {
        if i < self.param_names().len() {
            *self.value_mut(i) = value;
        }
        self.clamped()
    }

    /// Whether parameter `i` sets the latency (a change needs a new filter).
    pub fn is_structural(&self, i: usize) -> bool {
        matches!(self, Self::Limiter { .. }) && i == 2
    }

    /// Builds the filter for `channels` interleaved channels.
    pub fn build(&self, channels: usize) -> Box<dyn Filter + Send> {
        assert!(channels > 0, "no channel");
        match self.clamped() {
            Self::NoiseSuppression { max_attenuation_db } => {
                Box::new(Denoise::new(channels, max_attenuation_db))
            }
            Self::Expander {
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                range_db,
            } => Box::new(Expander::new(
                channels,
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                range_db,
            )),
            Self::Compressor {
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                knee_db,
                makeup_db,
            } => Box::new(Compressor::new(
                channels,
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                knee_db,
                makeup_db,
            )),
            Self::DeEsser {
                freq_hz,
                threshold_db,
                range_db,
            } => Box::new(DeEsser::new(channels, freq_hz, threshold_db, range_db)),
            Self::Limiter {
                ceiling_db,
                release_ms,
                lookahead_ms,
            } => Box::new(Limiter::new(channels, ceiling_db, release_ms, lookahead_ms)),
            Self::HighPass { freq_hz } => Box::new(HighPass::new(channels, freq_hz)),
            Self::Gain { db } => Box::new(Gain::new(channels, db)),
        }
    }
}

/// `name:key=value,key=value`, every parameter written.
impl fmt::Display for FilterSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", self.name())?;
        for (i, (name, value)) in self.param_names().iter().zip(self.values()).enumerate() {
            let sep = if i == 0 { "" } else { "," };
            write!(f, "{sep}{name}={value}")?;
        }
        Ok(())
    }
}

/// Parses `name` or `name:key=value,...`. Missing parameters take their
/// default; values out of range are clamped.
impl FromStr for FilterSpec {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let err = |what: String| ParseError(format!("{what} in filter `{s}`"));
        let (name, params) = s.split_once(':').unwrap_or((s, ""));
        let mut spec = Self::default_for(name.trim())
            .ok_or_else(|| err(format!("unknown filter `{}`", name.trim())))?;
        for param in params.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (key, value) = param
                .split_once('=')
                .ok_or_else(|| err(format!("`{param}` is not key=value")))?;
            let i = spec
                .param_names()
                .iter()
                .position(|n| *n == key.trim())
                .ok_or_else(|| err(format!("unknown parameter `{}`", key.trim())))?;
            let value: f32 = value
                .trim()
                .parse()
                .ok()
                .filter(|v: &f32| v.is_finite())
                .ok_or_else(|| err(format!("bad value `{}`", value.trim())))?;
            *spec.value_mut(i) = value;
        }
        Ok(spec.clamped())
    }
}

/// A filter's text form could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_form_round_trips() {
        for spec in FilterSpec::DEFAULTS {
            let text = spec.to_string();
            assert_eq!(text.parse::<FilterSpec>(), Ok(*spec), "{text}");
            assert!(spec.param_names().len() <= MAX_PARAMS);
        }
        let odd = FilterSpec::Compressor {
            threshold_db: -17.25,
            ratio: 2.5,
            attack_ms: 0.3,
            release_ms: 123.0,
            knee_db: 0.0,
            makeup_db: -1.5,
        };
        assert_eq!(odd.to_string().parse::<FilterSpec>(), Ok(odd));
        assert_eq!(FilterSpec::Gain { db: -3.5 }.to_string(), "gain:db=-3.5");
    }

    #[test]
    fn parse_fills_defaults_and_clamps() {
        let spec: FilterSpec = " compressor : ratio = 8 , makeup_db=0 ".parse().unwrap();
        let FilterSpec::Compressor {
            threshold_db,
            ratio,
            makeup_db,
            ..
        } = spec
        else {
            panic!("{spec:?}")
        };
        assert_eq!((threshold_db, ratio, makeup_db), (-20.0, 8.0, 0.0));
        assert_eq!("gain:db=1000".parse(), Ok(FilterSpec::Gain { db: 24.0 }));
        assert_eq!(
            "high_pass".parse(),
            Ok(FilterSpec::HighPass { freq_hz: 80.0 })
        );
        let nan = FilterSpec::Gain { db: f32::NAN }.clamped();
        assert_eq!(nan, FilterSpec::Gain { db: 0.0 });
        let gain = FilterSpec::Gain { db: 0.0 };
        assert_eq!(gain.with(0, 99.0), FilterSpec::Gain { db: 24.0 });
        assert_eq!(gain.with(1, 3.0), gain);
    }

    #[test]
    fn parse_rejects_garbage() {
        for bad in [
            "",
            "reverb",
            "gain:volume=1",
            "gain:db",
            "gain:db=loud",
            "gain:db=inf",
        ] {
            assert!(bad.parse::<FilterSpec>().is_err(), "{bad}");
        }
    }
}
