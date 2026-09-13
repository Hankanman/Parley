use log::info;
use std::sync::OnceLock;

/// Hardware capabilities for audio processing optimization
#[derive(Debug, Clone, PartialEq)]
pub struct HardwareProfile {
    pub cpu_cores: u8,
    pub has_gpu_acceleration: bool,
    pub gpu_type: GpuType,
    pub memory_gb: u8,
    pub performance_tier: PerformanceTier,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GpuType {
    None,
    Cuda,   // NVIDIA
    Vulkan, // AMD/Intel
}

#[derive(Debug, Clone, PartialEq)]
pub enum PerformanceTier {
    Low,    // CPU-only, limited resources
    Medium, // CPU-only but powerful, or basic GPU
    High,   // Dedicated GPU with good compute
    Ultra,  // High-end hardware with fast GPU
}

/// Adaptive Whisper configuration based on hardware
#[derive(Debug, Clone)]
pub struct AdaptiveWhisperConfig {
    pub beam_size: usize,
    pub temperature: f32,
    pub use_gpu: bool,
    pub max_threads: Option<usize>,
    pub chunk_size_preference: ChunkSizePreference,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChunkSizePreference {
    Fast,     // Smaller chunks for responsiveness
    Balanced, // Medium chunks for balance
    Quality,  // Larger chunks for accuracy
}

static HARDWARE_PROFILE: OnceLock<HardwareProfile> = OnceLock::new();

impl HardwareProfile {
    /// Get the detected hardware profile (cached after first call)
    pub fn detect() -> &'static HardwareProfile {
        HARDWARE_PROFILE.get_or_init(|| {
            let profile = Self::detect_hardware();
            info!("Detected hardware profile: {:?}", profile);
            profile
        })
    }

    /// Perform hardware detection
    fn detect_hardware() -> HardwareProfile {
        let cpu_cores = Self::detect_cpu_cores();
        let (has_gpu_acceleration, gpu_type) = Self::detect_gpu();
        let memory_gb = Self::detect_memory_gb();
        let performance_tier = Self::calculate_performance_tier(cpu_cores, &gpu_type, memory_gb);

        HardwareProfile {
            cpu_cores,
            has_gpu_acceleration,
            gpu_type,
            memory_gb,
            performance_tier,
        }
    }

    /// Detect number of CPU cores
    fn detect_cpu_cores() -> u8 {
        std::thread::available_parallelism()
            .map(|n| n.get().min(255) as u8)
            .unwrap_or(4) // Default to 4 cores
    }

    /// Detect GPU acceleration capabilities.
    ///
    /// The compiled Cargo feature (`cuda` / `vulkan` / `hipblas`, see
    /// `Cargo.toml` `[features]`) is the source of truth: it determines
    /// whether whisper-rs was actually built with GPU support linked in.
    /// Filesystem/env heuristics are unrelated to what got compiled (e.g. a
    /// CPU-only build can still have `/usr/local/cuda` installed for other
    /// tooling, and a Vulkan build's runtime-only `libvulkan.so.1` doesn't
    /// match the dev-package-only unversioned `libvulkan.so` path we used to
    /// probe for) so they are used only as a secondary sanity check — logged
    /// as a warning, never as the decision itself. A CPU-only build must
    /// never report a GPU.
    fn detect_gpu() -> (bool, GpuType) {
        #[cfg(feature = "cuda")]
        {
            if !Self::has_cuda_support() {
                log::warn!(
                    "Built with the 'cuda' feature but no CUDA installation was found \
                     via CUDA_PATH/CUDA_HOME or /usr/local/cuda; GPU init may fail at runtime."
                );
            }
            return (true, GpuType::Cuda);
        }

        #[cfg(feature = "hipblas")]
        {
            // AMD ROCm HIP - treated as the Vulkan-equivalent GPU tier since
            // there is no dedicated HIP variant of `GpuType`.
            return (true, GpuType::Vulkan);
        }

        #[cfg(feature = "vulkan")]
        {
            if !Self::has_vulkan_support() {
                log::warn!(
                    "Built with the 'vulkan' feature but no Vulkan runtime library was found \
                     under the probed paths; GPU init may fail at runtime."
                );
            }
            return (true, GpuType::Vulkan);
        }

        // No GPU-accelerated backend was compiled in - CPU-only build.
        #[allow(unreachable_code)]
        (false, GpuType::None)
    }

    /// Detect available system memory in GB.
    ///
    /// Reads physical memory from `/proc/meminfo` (`MemTotal`) on Linux. The
    /// `MEMORY_GB` env var can still be set to override this (useful for
    /// testing/tuning); it takes precedence when present. Falls back to a
    /// conservative 8 GB only when neither source is available/parseable.
    fn detect_memory_gb() -> u8 {
        if let Ok(mem_str) = std::env::var("MEMORY_GB") {
            if let Ok(gb) = mem_str.parse() {
                return gb;
            }
        }

        match std::fs::read_to_string("/proc/meminfo") {
            Ok(contents) => Self::parse_meminfo_total_gb(&contents).unwrap_or(8),
            Err(_) => 8, // Conservative default when /proc/meminfo is unreadable
        }
    }

    /// Parse the `MemTotal:` line out of `/proc/meminfo` content (kB) and
    /// convert it to whole gigabytes (rounded to the nearest GB, minimum 1).
    fn parse_meminfo_total_gb(meminfo: &str) -> Option<u8> {
        let kb: u64 = meminfo
            .lines()
            .find(|line| line.starts_with("MemTotal:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())?;

        let gb = (kb as f64 / 1024.0 / 1024.0).round() as u64;
        Some(gb.max(1).min(255) as u8)
    }

    /// Calculate performance tier based on hardware
    fn calculate_performance_tier(
        cpu_cores: u8,
        gpu_type: &GpuType,
        memory_gb: u8,
    ) -> PerformanceTier {
        match gpu_type {
            GpuType::Cuda => {
                if memory_gb >= 16 && cpu_cores >= 8 {
                    PerformanceTier::Ultra
                } else {
                    PerformanceTier::High
                }
            }
            GpuType::Vulkan => {
                if memory_gb >= 12 && cpu_cores >= 6 {
                    PerformanceTier::High
                } else {
                    PerformanceTier::Medium
                }
            }
            GpuType::None => {
                // No compiled GPU backend: still avoid dumping every
                // reasonably-specced CPU-only machine into the lowest tier.
                // A stock 8GB/4-core box is "Medium" (decent beam size),
                // and it takes genuinely low-spec hardware (few cores and/or
                // little memory) to land in "Low".
                if cpu_cores >= 4 && memory_gb >= 8 {
                    PerformanceTier::Medium
                } else {
                    PerformanceTier::Low
                }
            }
        }
    }

    /// Secondary sanity check only (see `detect_gpu`), not used to decide
    /// `GpuType` - only consulted when the `cuda` feature is compiled in.
    #[cfg_attr(not(feature = "cuda"), allow(dead_code))]
    fn has_cuda_support() -> bool {
        // Check for CUDA environment or libraries
        std::env::var("CUDA_PATH").is_ok()
            || std::env::var("CUDA_HOME").is_ok()
            || std::path::Path::new("/usr/local/cuda").exists()
    }

    /// Secondary sanity check only (see `detect_gpu`), not used to decide
    /// `GpuType` - only consulted when the `vulkan` feature is compiled in.
    ///
    /// Probes for the *runtime* loader library (`libvulkan.so.1`), which is
    /// what actually ships on end-user machines - the unversioned
    /// `libvulkan.so` symlink is only installed by `-dev`/`-devel` packages.
    /// Covers Debian/Ubuntu multiarch, plain `/usr/lib`, and Fedora/RHEL's
    /// `/usr/lib64` layout.
    #[cfg_attr(not(feature = "vulkan"), allow(dead_code))]
    fn has_vulkan_support() -> bool {
        std::env::var("VULKAN_SDK").is_ok()
            || [
                "/usr/lib/x86_64-linux-gnu/libvulkan.so.1",
                "/usr/lib/x86_64-linux-gnu/libvulkan.so",
                "/usr/lib64/libvulkan.so.1",
                "/usr/lib64/libvulkan.so",
                "/usr/lib/libvulkan.so.1",
                "/usr/lib/libvulkan.so",
            ]
            .iter()
            .any(|p| std::path::Path::new(p).exists())
    }

    /// Generate adaptive Whisper configuration based on hardware
    pub fn get_whisper_config(&self) -> AdaptiveWhisperConfig {
        match self.performance_tier {
            PerformanceTier::Ultra => AdaptiveWhisperConfig {
                beam_size: 5, // Maximum quality
                temperature: 0.1,
                use_gpu: self.has_gpu_acceleration,
                max_threads: Some(self.cpu_cores.min(8) as usize),
                chunk_size_preference: ChunkSizePreference::Quality,
            },
            PerformanceTier::High => AdaptiveWhisperConfig {
                beam_size: 3, // High quality
                temperature: 0.2,
                use_gpu: self.has_gpu_acceleration,
                max_threads: Some(self.cpu_cores.min(6) as usize),
                chunk_size_preference: ChunkSizePreference::Balanced,
            },
            PerformanceTier::Medium => AdaptiveWhisperConfig {
                beam_size: 2, // Balanced
                temperature: 0.3,
                use_gpu: self.has_gpu_acceleration,
                max_threads: Some(self.cpu_cores.min(4) as usize),
                chunk_size_preference: ChunkSizePreference::Balanced,
            },
            PerformanceTier::Low => AdaptiveWhisperConfig {
                beam_size: 1, // Fast processing
                temperature: 0.4,
                use_gpu: false, // Force CPU to avoid GPU overhead on weak hardware
                max_threads: Some(2),
                chunk_size_preference: ChunkSizePreference::Fast,
            },
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hardware_detection() {
        let profile = HardwareProfile::detect();
        assert!(profile.cpu_cores > 0);
        // Performance optimization: remove println! from tests
        log::debug!("Detected profile: {:?}", profile);
    }

    #[test]
    fn test_whisper_config_generation() {
        let profile = HardwareProfile::detect();
        let config = profile.get_whisper_config();

        assert!(config.beam_size >= 1 && config.beam_size <= 5);
        assert!(config.temperature >= 0.0 && config.temperature <= 1.0);

        // Performance optimization: remove println! from tests
        log::debug!("Generated config: {:?}", config);
    }

    #[test]
    fn test_performance_tier_logic() {
        // Test different hardware combinations
        let low_tier = HardwareProfile::calculate_performance_tier(2, &GpuType::None, 4);
        assert_eq!(low_tier, PerformanceTier::Low);

        let high_tier = HardwareProfile::calculate_performance_tier(8, &GpuType::Cuda, 16);
        assert_eq!(high_tier, PerformanceTier::Ultra);
    }

    #[test]
    fn test_cpu_only_stock_hardware_is_not_lowest_tier() {
        // Regression test for issue #20: a stock CPU-only machine (8GB RAM,
        // 4 cores) used to land in PerformanceTier::Low (beam_size 1,
        // temperature 0.4) because the old logic required >= 16GB to reach
        // Medium. It should now land in Medium.
        let tier = HardwareProfile::calculate_performance_tier(4, &GpuType::None, 8);
        assert_eq!(tier, PerformanceTier::Medium);

        // Genuinely low-spec hardware should still be Low.
        let tier = HardwareProfile::calculate_performance_tier(2, &GpuType::None, 4);
        assert_eq!(tier, PerformanceTier::Low);

        // Low core count but plenty of memory is still Low.
        let tier = HardwareProfile::calculate_performance_tier(2, &GpuType::None, 32);
        assert_eq!(tier, PerformanceTier::Low);
    }

    #[test]
    fn test_gpu_tier_never_below_medium() {
        // A compiled-in GPU feature (Vulkan/hipblas-mapped-to-Vulkan) should
        // never be shoved down to the Low tier, even on weak CPU/memory,
        // since Low forces use_gpu: false and beam_size: 1.
        let tier = HardwareProfile::calculate_performance_tier(1, &GpuType::Vulkan, 1);
        assert_ne!(tier, PerformanceTier::Low);

        let tier = HardwareProfile::calculate_performance_tier(1, &GpuType::Cuda, 1);
        assert_ne!(tier, PerformanceTier::Low);
    }

    #[test]
    fn test_parse_meminfo_total_gb() {
        let sample = "MemTotal:       16384000 kB\n\
                       MemFree:         1234567 kB\n\
                       MemAvailable:    8765432 kB\n";
        // 16384000 kB / 1024 / 1024 = ~15.625 GB, rounds to 16.
        assert_eq!(HardwareProfile::parse_meminfo_total_gb(sample), Some(16));
    }

    #[test]
    fn test_parse_meminfo_total_gb_small_and_odd_spacing() {
        let sample = "MemTotal:   8000000 kB\n";
        // 8000000 kB / 1024 / 1024 = ~7.629 GB, rounds to 8.
        assert_eq!(HardwareProfile::parse_meminfo_total_gb(sample), Some(8));
    }

    #[test]
    fn test_parse_meminfo_total_gb_missing_field() {
        let sample = "MemFree: 1234 kB\nMemAvailable: 5678 kB\n";
        assert_eq!(HardwareProfile::parse_meminfo_total_gb(sample), None);
    }

    #[test]
    fn test_parse_meminfo_total_gb_malformed() {
        let sample = "MemTotal: not-a-number kB\n";
        assert_eq!(HardwareProfile::parse_meminfo_total_gb(sample), None);
    }
}
