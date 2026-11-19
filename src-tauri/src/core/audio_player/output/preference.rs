//! Préférence runtime du backend audio.
//!
//! Stockée dans un atomic global pour être lue rapidement par
//! `play_file_thread` sans accès BDD (le thread audio n'a pas accès au pool
//! tokio). La valeur est synchronisée avec la BDD au boot
//! (`init_from_settings`) et à chaque toggle utilisateur via la Tauri
//! command `set_wasapi_exclusive_preference`.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use super::types::AudioBackend;

/// Mode de sortie choisi par l'utilisateur :
/// `0` = partagé (CPAL), `1` = partagé IAudioClient3 faible latence,
/// `2` = exclusif (bit-perfect). Remplace l'ancien booléen `WASAPI_EXCLUSIVE`
/// tout en restant compatible avec la commande historique `set_wasapi_exclusive`.
static OUTPUT_MODE: AtomicU8 = AtomicU8::new(0);

/// Épisode de podcast en cours : sortie partagée, la seule où la vitesse de lecture s'applique.
static SHARED_ONLY: AtomicBool = AtomicBool::new(false);

pub fn set_shared_only(on: bool) {
    SHARED_ONLY.store(on, Ordering::Relaxed);
}

/// Préférence « DSD natif (DoP) ». Quand activée ET que WASAPI exclusive est
/// actif ET que le DAC accepte le format porteur, les fichiers DSD sont
/// envoyés en DoP (DSD over PCM) au lieu d'être convertis en PCM.
static DSD_DOP: AtomicBool = AtomicBool::new(false);

/// `true` si l'utilisateur a activé le DoP. La décision finale (DoP vs
/// DSD2PCM) dépend AUSSI de WASAPI exclusive actif + compat DAC, vérifiée
/// au moment de la lecture.
pub fn dop_enabled() -> bool {
    // Windows : DoP via WASAPI exclusive. Linux : DoP via ALSA hw exclusif.
    // macOS : DoP via CoreAudio + hog mode (seule voie DSD native du système).
    // Sans accès direct à la carte (Flatpak), le DSD passe par la conversion en PCM.
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    {
        return DSD_DOP.load(Ordering::Relaxed) && crate::core::sandbox::exclusive_possible();
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// Met à jour la préférence DoP (effet au prochain morceau DSD lancé).
pub fn set_dop_enabled(enabled: bool) {
    DSD_DOP.store(enabled, Ordering::Relaxed);
    log::info!("🎚️  DSD natif (DoP) preference : {}", if enabled { "ON" } else { "OFF" });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_maps_to_backend() {
        set_output_mode(0);
        assert_eq!(current_preference(), AudioBackend::CpalShared);
        set_output_mode(1);
        // IAudioClient3 n'existe que sous Windows sur les autres OS on retombe
        // sur le partagé (aucun backend exclusif n'est selectionné par erreur).
        #[cfg(not(target_os = "windows"))]
        assert_eq!(current_preference(), AudioBackend::CpalShared);
        set_output_mode(2);
        assert_eq!(current_preference(), AudioBackend::platform_exclusive().unwrap_or(AudioBackend::CpalShared));
        // Remet le partagé pour ne pas polluer les autres tests du binaire.
        set_output_mode(0);
    }

    #[test]
    fn legacy_boolean_still_maps() {
        set_wasapi_exclusive(true);
        assert_eq!(output_mode(), 2);
        set_wasapi_exclusive(false);
        assert_eq!(output_mode(), 0);
    }
}

/// Backend préféré par l'utilisateur : le backend exclusif de la plateforme
/// quand la sortie exclusive est activée, `CpalShared` sinon.
///
/// Le réglage est unique et partagé par les trois OS (clé BDD historique
/// `wasapi_exclusive`, conservée pour ne pas perdre le choix des utilisateurs
/// Windows existants) ; c'est la plateforme qui décide du backend concret.
pub fn current_preference() -> AudioBackend {
    // Un podcast (vitesse de lecture) impose le chemin partagé CPAL, quel que
    // soit le réglage : seul lui applique l'étirement temporel.
    if SHARED_ONLY.load(Ordering::Relaxed) {
        return AudioBackend::CpalShared;
    }
    match OUTPUT_MODE.load(Ordering::Relaxed) {
        2 => AudioBackend::platform_exclusive().unwrap_or(AudioBackend::CpalShared),
        1 => {
            #[cfg(target_os = "windows")]
            {
                AudioBackend::WasapiLowLatency
            }
            #[cfg(not(target_os = "windows"))]
            {
                // Pas d'IAudioClient3 hors Windows : repli sur le partagé.
                AudioBackend::CpalShared
            }
        }
        _ => AudioBackend::CpalShared,
    }
}

/// Met à jour le mode de sortie (0 = partagé, 1 = faible latence, 2 = exclusif).
/// Effet au PROCHAIN morceau lu ; les lectures en cours ne sont pas interrompues.
pub fn set_output_mode(mode: u8) {
    let mode = mode.min(2);
    OUTPUT_MODE.store(mode, Ordering::Relaxed);
    log::info!("🎚️  Mode de sortie : {}", match mode {
        1 => "partagé faible latence (IAudioClient3)",
        2 => "exclusif (bit-perfect)",
        _ => "partagé (CPAL)",
    });
}

/// Compatibilité avec l'ancienne commande `set_wasapi_exclusive_preference`.
pub fn set_wasapi_exclusive(enabled: bool) {
    set_output_mode(if enabled { 2 } else { 0 });
}

/// Mode courant sous forme numérique (0/1/2), pour l'UI et les tests.
pub fn output_mode() -> u8 {
    OUTPUT_MODE.load(Ordering::Relaxed)
}
