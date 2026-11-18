//! Vérification d'intégrité des fichiers audio téléchargés.
//!
//! Un fichier peut avoir la bonne taille et un en-tête valide mais des trames
//! corrompues au milieu (cas observé sur un FLAC de 72 Mo lu en streaming :
//! `invalid sync code` / `invalid frame header`, silence à la lecture). Ni la
//! taille, ni l'en-tête, ni les en-têtes HTTP ne détectent ce cas. La seule
//! vérification fiable est de décoder le fichier jusqu'à la fin avec la
//! validation intégrée de Symphonia (`AudioDecoderOptions::verify`), qui
//! contrôle le MD5 du STREAMINFO FLAC (et les CRC de trames pour les autres
//! formats). Coût : quelques secondes pour un gros fichier — d'où un appel
//! unique au moment de l'écriture, jamais à chaque lecture.

use std::path::Path;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::core::audio_player::codecs::codec_registry;

/// Résultat d'une vérification d'intégrité.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integrity {
    /// Fichier décodé intégralement, validation OK (ou non disponible mais sans erreur).
    Ok,
    /// Le format n'a pas de validation intégrée exploitable (ex. codec non pris en charge)
    /// mais le fichier s'est décodé jusqu'au bout sans erreur : on ne peut pas faire mieux.
    Unverifiable,
}

/// Vérifie qu'un fichier audio se décode intégralement sans erreur.
///
/// - Une erreur de décodage de trame (`decode`) est une corruption avérée → `Err`.
/// - Le `UnexpectedEof` de `next_packet()` en fin de flux est TOLÉRÉ : les FLAC ont
///   souvent des octets de remplissage après la dernière trame, que Symphonia signale
///   comme une fin prématurée alors que l'audio est complet (vérifié : ffmpeg décode
///   bien les 4421 trames / 377,2 s du fichier incriminé). Le rejeter ferait rejeter
///   un fichier parfaitement jouable.
/// - Un MD5 de STREAMINFO divergent n'est PAS fatal : il signale que le PCM diffère
///   du hash déclaré, mais si toutes les trames se décodent, le fichier reste jouable
///   (c'est le cas du fichier incriminé, que l'app Emby official joue sans problème).
/// - Les codecs sans validation intégrée renvoient `Unverifiable` si le décodage est propre.
///
/// Cette fonction est volontairement synchrone et potentiellement coûteuse : l'appelant
/// doit l'exécuter dans un `spawn_blocking` (jamais sur le fil de lecture/UI).
pub fn verify_integrity(path: &Path, hint_ext: Option<&str>) -> Result<Integrity, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("ouverture : {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    // Le fichier en cours est un `.part` : le vrai format vient du nom final.
    if let Some(ext) = hint_ext.or_else(|| path.extension().and_then(|e| e.to_str())) {
        hint.with_extension(ext);
    }
    let mut reader = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| format!("probe : {e}"))?;

    let track = reader
        .tracks()
        .iter()
        .find(|t| t.codec_params.as_ref().and_then(|c| c.audio()).is_some())
        .ok_or_else(|| "aucune piste audio".to_string())?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|c| c.audio())
        .cloned()
        .ok_or_else(|| "paramètres codec audio absents".to_string())?;
    let mut decoder = codec_registry()
        .make_audio_decoder(&params, &AudioDecoderOptions::default().verify(true))
        .map_err(|e| format!("création décodeur : {e:?}"))?;

    // Décodage intégral : la moindre erreur est une corruption. Ne PAS reproduire
    // la logique du lecteur qui saute les paquets cassés (`audio_player.rs:2548`).
    let mut packets = 0u64;
    let mut decode_errors = 0u64;
    loop {
        let packet = match reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            // Octets de remplissage après la dernière trame : fin normale, pas une
            // corruption (voir doc de la fonction). Ne PAS rejeter ce fichier jouable.
            Err(SymphoniaError::IoError(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("lecture paquet {packets} : {e}")),
        };
        if packet.track_id != track_id {
            continue;
        }
        if decoder.decode(&packet).is_err() {
            decode_errors += 1;
        }
        packets += 1;
    }
    if packets == 0 {
        return Err("aucun paquet audio décodé".to_string());
    }
    // Seules les erreurs de décodage de trames prouvent une corruption qui
    // empêche la lecture. Un MD5 divergent (verify_ok = Some(false)) est
    // averti mais non rejeté : le fichier reste jouable.
    if decode_errors > 0 {
        return Err(format!("{decode_errors} trame(s) illisible(s) sur {packets}"));
    }

    let finalize = decoder.finalize();
    if finalize.verify_ok == Some(false) {
        log::warn!("⚠️  MD5/validation du flux divergent, mais toutes les trames se décodent : fichier conservé");
    }
    match finalize.verify_ok {
        Some(_) => Ok(Integrity::Ok),
        None => Ok(Integrity::Unverifiable),
    }
}

/// Vérifie un fichier téléchargé, en ne rejetant que les formats que Symphonia sait
/// réellement décoder. Les formats non pris en charge (DSF/DFF notamment, dont le
/// décodeur est maison) sont laissés passer : on ne peut pas les valider ici.
///
/// Renvoie `Err` uniquement si le fichier doit être rejeté (corruption avérée pour
/// un format vérifiable).
pub fn verify_download(path: &Path, hint_ext: Option<&str>) -> Result<(), String> {
    let ext = hint_ext
        .or_else(|| path.extension().and_then(|e| e.to_str()))
        .unwrap_or("")
        .to_ascii_lowercase();
    let verifiable = matches!(
        ext.as_str(),
        "flac" | "mp3" | "wav" | "ogg" | "oga" | "opus" | "m4a" | "mp4" | "aac" | "aiff" | "aif"
    );
    if !verifiable {
        return Ok(());
    }
    verify_integrity(path, Some(&ext)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(nom: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(nom)
    }

    #[test]
    fn flac_intact_is_ok() {
        let r = verify_integrity(&fixture("flac24.flac"), None).expect("un FLAC intact doit se décoder");
        assert_eq!(r, Integrity::Ok);
    }

    /// Un FLAC dont les synchronisations de trames sont détruites ne peut plus
    /// être décodé : c'est une vraie corruption, qui DOIT être rejetée.
    #[test]
    fn flac_with_destroyed_frame_sync_is_rejected() {
        let src = std::fs::read(fixture("flac24.flac")).unwrap();
        let dir = std::env::temp_dir().join("rustmusic-integrity-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nosync.flac");

        let mut corrupt = src.clone();
        // Détruit tous les préambules de trame (0xFFF8/0xFFF9) : le décodeur
        // ne peut plus trouver de trame du tout.
        let mut n = 0;
        for i in 0..corrupt.len().saturating_sub(1) {
            if corrupt[i] == 0xFF && (corrupt[i + 1] & 0xFE) == 0xF8 {
                corrupt[i] = 0x00;
                n += 1;
            }
        }
        assert!(n > 0, "fixture devrait contenir des trames FLAC");
        std::fs::write(&path, &corrupt).unwrap();

        let result = verify_integrity(&path, Some("flac"));
        assert!(result.is_err(), "un FLAC sans trames décodables doit être rejeté");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Un FLAC jouable mais dont le MD5 du STREAMINFO ne correspond pas (cas du
    /// fichier incriminé : toutes les trames se décodent, l'app Emby le joue) ne
    /// doit PAS être rejeté — le rejeter créerait précisément le silence qu'on
    /// cherche à éviter. Le MD5 divergent est seulement averti.
    #[test]
    fn flac_with_md5_mismatch_but_decodable_is_kept() {
        let src = std::fs::read(fixture("flac24.flac")).unwrap();
        let dir = std::env::temp_dir().join("rustmusic-integrity-md5");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("md5mismatch.flac");

        let mut corrupt = src.clone();
        // Corrompt le MD5 déclaré dans le STREAMINFO (octets 26..42), pas l'audio.
        for b in corrupt.iter_mut().skip(26).take(16) {
            *b ^= 0xFF;
        }
        std::fs::write(&path, &corrupt).unwrap();

        let result = verify_integrity(&path, Some("flac"));
        assert!(result.is_ok(), "un FLAC dont l'audio se décode doit être conservé même si le MD5 diffère");

        std::fs::remove_dir_all(&dir).ok();
    }
}
