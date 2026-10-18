// ------------ FPS Unlocker Module ------------
// Wires the FPS unlocker together and exposes its shared section. The stub, scanner and PE reader only compile with
// the `stub` feature, because they end up inside the DLL that runs in the game.

pub mod shared;

#[cfg(all(windows, feature = "stub"))]
mod pe;
#[cfg(all(windows, feature = "stub"))]
mod scan;
#[cfg(all(windows, feature = "stub"))]
mod stub;

#[cfg(windows)]
pub mod section {
    use core::sync::atomic::Ordering;

    pub use crate::common::section::Signal;

    use super::shared::Shared;

    pub type SectionView = crate::common::section::SectionView<Shared>;

    pub fn publish_settings(section: &SectionView, target_fps: i32, background_fps: i32) {
        let shared = section.shared();
        shared.target_fps.store(target_fps, Ordering::Relaxed);
        shared
            .background_fps
            .store(background_fps, Ordering::Relaxed);
    }
}
