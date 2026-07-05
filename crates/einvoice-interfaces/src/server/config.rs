//! Server configuration: environment variables with hardware-derived defaults.
//!
//! Every knob is an environment variable; defaults come from what the machine
//! (or container) actually offers. A malformed value is a startup error —
//! never a silent fallback.
//!
//! | Variable                | Default                                        |
//! |-------------------------|------------------------------------------------|
//! | `KRAB_ADDR`             | `0.0.0.0:8080`                                 |
//! | `KRAB_WORKERS`          | available parallelism (cgroup-aware)           |
//! | `KRAB_MEM_BUDGET_BYTES` | detected memory x 1/2 (cgroup v2 limit first)  |
//! | `KRAB_MEM_BLOWUP`       | `7` (measured peak-memory multiplier)          |
//!
//! # Structure
//!
//! [`Config::from_env`] is the thin I/O entry point; [`Config::resolve`] is
//! the pure core taking an env lookup plus the detected hardware values, so
//! every rule is unit-testable without touching process environment (global
//! mutable state). [`parse_cgroup_max`] and [`parse_meminfo`] are the pure
//! parsers behind memory detection.
//!
//! # Behavior
//!
//! Memory detection prefers the cgroup v2 limit (`/sys/fs/cgroup/memory.max`)
//! because inside a container `/proc/meminfo` shows the *host's* RAM — sizing
//! the budget from it would invite the OOM killer. The `"max"` sentinel (no
//! container limit) falls through to `MemTotal`. If neither source is
//! readable and `KRAB_MEM_BUDGET_BYTES` is unset, startup fails.
//!
//! # Testing
//!
//! Unit tests cover the defaults, every override, rejection of malformed and
//! zero values, the missing-detection error, and both parsers over fixtures.

/// Runtime configuration for `krab-server`. See the module docs for the
/// variables and defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Listen address, `host:port`.
    pub addr: String,
    /// Worker threads sharing the accept loop.
    pub workers: usize,
    /// Total bytes the memory gate may reserve at once.
    pub mem_budget_bytes: u64,
    /// Reservation multiplier: a request reserves `Content-Length x blowup`.
    pub mem_blowup: u64,
}

/// A configuration problem that must stop startup.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// An environment variable held an unusable value.
    #[error("invalid {var}={value:?}: {reason}")]
    Invalid {
        /// The offending variable name.
        var: &'static str,
        /// The raw value found.
        value: String,
        /// Why it was rejected.
        reason: String,
    },
    /// No memory limit could be detected and no explicit budget was given.
    #[error(
        "cannot detect system memory (no cgroup limit, no /proc/meminfo); \
         set KRAB_MEM_BUDGET_BYTES"
    )]
    NoMemorySource,
}

impl Config {
    /// Resolves the configuration from an environment `lookup` and the
    /// detected hardware: `detected_mem` (bytes, `None` when undetectable)
    /// and `detected_cores`.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] for malformed or zero values;
    /// [`ConfigError::NoMemorySource`] when no budget can be derived.
    pub fn resolve(
        lookup: impl Fn(&str) -> Option<String>,
        detected_mem: Option<u64>,
        detected_cores: usize,
    ) -> Result<Self, ConfigError> {
        let workers = match lookup("KRAB_WORKERS") {
            Some(v) => parse_nonzero("KRAB_WORKERS", &v)? as usize,
            None => detected_cores,
        };
        let mem_budget_bytes = match lookup("KRAB_MEM_BUDGET_BYTES") {
            Some(v) => parse_nonzero("KRAB_MEM_BUDGET_BYTES", &v)?,
            None => detected_mem.ok_or(ConfigError::NoMemorySource)? / 2,
        };
        let mem_blowup = match lookup("KRAB_MEM_BLOWUP") {
            Some(v) => parse_nonzero("KRAB_MEM_BLOWUP", &v)?,
            // Measured peak RSS over the transform path (inline strings +
            // boxed interior containers): worst ~6.1x the body for small
            // line-dense documents, ~5.3x marginal at scale; 7 adds headroom.
            // Ratios measured with glibc — re-measure in the musl container
            // before tightening further.
            None => 7,
        };
        Ok(Config {
            addr: lookup("KRAB_ADDR").unwrap_or_else(|| "0.0.0.0:8080".into()),
            workers,
            mem_budget_bytes,
            mem_blowup,
        })
    }

    /// Reads the real environment and hardware (cgroup v2 limit, then
    /// `/proc/meminfo`; `std::thread::available_parallelism`).
    ///
    /// # Errors
    ///
    /// Same as [`Config::resolve`].
    pub fn from_env() -> Result<Self, ConfigError> {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Config::resolve(|k| std::env::var(k).ok(), detect_memory(), cores)
    }
}

/// Parses a positive integer environment value; zero is as fatal as garbage
/// (zero workers serve nothing, a zero budget admits nothing).
fn parse_nonzero(var: &'static str, value: &str) -> Result<u64, ConfigError> {
    let invalid = |reason: &str| ConfigError::Invalid {
        var,
        value: value.into(),
        reason: reason.into(),
    };
    let n: u64 = value
        .trim()
        .parse()
        .map_err(|_| invalid("expected a positive integer"))?;
    if n == 0 {
        return Err(invalid("must be greater than zero"));
    }
    Ok(n)
}

/// Detects usable memory: the cgroup v2 limit when the process is confined
/// (inside a container `/proc/meminfo` shows the host), else `MemTotal`.
fn detect_memory() -> Option<u64> {
    std::fs::read_to_string("/sys/fs/cgroup/memory.max")
        .ok()
        .and_then(|s| parse_cgroup_max(&s))
        .or_else(|| {
            std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|s| parse_meminfo(&s))
        })
}

/// Parses `/sys/fs/cgroup/memory.max`: a byte count, or the `"max"` sentinel
/// (unlimited) which yields `None`.
pub fn parse_cgroup_max(contents: &str) -> Option<u64> {
    contents.trim().parse().ok()
}

/// Parses `/proc/meminfo`, returning `MemTotal` in bytes.
pub fn parse_meminfo(contents: &str) -> Option<u64> {
    let line = contents.lines().find(|l| l.starts_with("MemTotal:"))?;
    // Format: `MemTotal:       65758096 kB`
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn test_resolve_no_env_uses_hardware_defaults() {
        let cfg = Config::resolve(env(&[]), Some(64 * GIB), 16).expect("defaults resolve");
        assert_eq!(
            cfg,
            Config {
                addr: "0.0.0.0:8080".into(),
                workers: 16,
                mem_budget_bytes: 32 * GIB, // half of detected
                mem_blowup: 7,
            }
        );
    }

    #[test]
    fn test_resolve_env_overrides_win() {
        let cfg = Config::resolve(
            env(&[
                ("KRAB_ADDR", "127.0.0.1:9000"),
                ("KRAB_WORKERS", "4"),
                ("KRAB_MEM_BUDGET_BYTES", "1000000"),
                ("KRAB_MEM_BLOWUP", "3"),
            ]),
            Some(64 * GIB),
            16,
        )
        .expect("overrides resolve");
        assert_eq!(
            cfg,
            Config {
                addr: "127.0.0.1:9000".into(),
                workers: 4,
                mem_budget_bytes: 1_000_000,
                mem_blowup: 3,
            }
        );
    }

    #[test]
    fn test_resolve_explicit_budget_needs_no_detection() {
        let cfg = Config::resolve(env(&[("KRAB_MEM_BUDGET_BYTES", "1000000")]), None, 8)
            .expect("explicit budget suffices");
        assert_eq!(cfg.mem_budget_bytes, 1_000_000);
    }

    #[test]
    fn test_resolve_no_detection_and_no_budget_is_no_memory_source() {
        let err = Config::resolve(env(&[]), None, 8).expect_err("nothing to size budget from");
        assert_eq!(err, ConfigError::NoMemorySource);
    }

    #[test]
    fn test_resolve_malformed_number_is_invalid() {
        let err = Config::resolve(env(&[("KRAB_WORKERS", "lots")]), Some(GIB), 8)
            .expect_err("not a number");
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    var: "KRAB_WORKERS",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn test_resolve_zero_workers_is_invalid() {
        let err =
            Config::resolve(env(&[("KRAB_WORKERS", "0")]), Some(GIB), 8).expect_err("zero workers");
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    var: "KRAB_WORKERS",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn test_resolve_zero_budget_is_invalid() {
        let err = Config::resolve(env(&[("KRAB_MEM_BUDGET_BYTES", "0")]), Some(GIB), 8)
            .expect_err("zero budget admits nothing");
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    var: "KRAB_MEM_BUDGET_BYTES",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn test_resolve_zero_blowup_is_invalid() {
        let err = Config::resolve(env(&[("KRAB_MEM_BLOWUP", "0")]), Some(GIB), 8)
            .expect_err("zero blowup reserves nothing");
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    var: "KRAB_MEM_BLOWUP",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn test_parse_cgroup_max_number_is_bytes() {
        assert_eq!(parse_cgroup_max("8589934592\n"), Some(8 * GIB));
    }

    #[test]
    fn test_parse_cgroup_max_sentinel_is_none() {
        assert_eq!(parse_cgroup_max("max\n"), None);
    }

    #[test]
    fn test_parse_cgroup_max_garbage_is_none() {
        assert_eq!(parse_cgroup_max("not-a-limit"), None);
    }

    #[test]
    fn test_parse_meminfo_returns_memtotal_bytes() {
        let fixture = "MemTotal:       65758096 kB\n\
                       MemFree:        49386412 kB\n\
                       MemAvailable:   57021152 kB\n";
        assert_eq!(parse_meminfo(fixture), Some(65_758_096 * 1024));
    }

    #[test]
    fn test_parse_meminfo_without_memtotal_is_none() {
        assert_eq!(parse_meminfo("MemFree: 12 kB\n"), None);
        assert_eq!(parse_meminfo(""), None);
    }
}
