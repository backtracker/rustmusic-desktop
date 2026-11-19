//! Backend WASAPI « faible latence » (Windows uniquement).
//!
//! Ce backend utilise **IAudioClient3** en mode partagé à période réduite :
//! `GetSharedModeEnginePeriod` négocie la période minimale que le moteur audio
//! accepte, puis `InitializeSharedAudioStream` ouvre le flux avec cette période.
//!
//! Différence avec les deux autres backends :
//! - **CPAL shared** : flux partagé à la période par défaut du device. C'est le
//!   chemin historique.
//! - **IAudioClient3 shared (ce module)** : même mixage système, mais période
//!   réduite → latence comparable à l'exclusif, tout en gardant les sons système.
//!   ⚠️ Ce n'est **pas** du bit-perfect : le moteur mélange et peut convertir.
//! - **WASAPI exclusive** : bit-perfect, mais coupe tous les autres sons.
//!
//! La source f32 est produite par `cpal_symphonia::render_fn` (déjà utilisé par
//! le backend CPAL) : LiveDecode / FullBuffer, seek, enchaînement, vitesse,
//! volume, ReplayGain. Ce module ne fait que convertir ces f32 dans le format
//! du mix et les écrire dans le tampon IAudioClient3.
//!
//! ⚠️ COM/WASAPI : les objets (`AudioClient`…) ne sont pas `Send`. Toute
//! l'initialisation ET la boucle de rendu se font donc dans LE MÊME thread.
//! Le `Consumer` n'est envoyé à ce thread qu'après une négociation réussie,
//! pour pouvoir le rendre à l'appelant en cas d'échec (repli CPAL).

#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ringbuf::traits::Consumer;
use wasapi::{initialize_mta, DeviceEnumerator, SampleType, TimingMode, WaveFormat};

use super::traits::{AudioOutput, AudioOutputError};
use super::types::{AudioBackend, PlaybackAtomics, SymphoniaSharedState};
use crate::core::audio_player::audio_output_wasapi::resolve_wasapi_device;

/// Durée de période visée, en ms. Le moteur choisit la plus petite valeur
/// supportée ≥ cette cible (bornée par min/max et alignée sur fundamental).
const TARGET_PERIOD_MS: f64 = 10.0;

/// Attente maximale de la négociation IAudioClient3 au démarrage.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Calcule la période (en frames) à demander à IAudioClient3.
///
/// `IAudioClient3::GetSharedModeEnginePeriod` renvoie une plage et un pas
/// fondamental : la période doit être un multiple entier de `fundamental`.
/// On vise ~10 ms, borné dans [min, max], puis aligné vers le bas sur le pas.
pub(crate) fn choose_period(
    periods: wasapi::SharedModeEnginePeriod,
    sample_rate: u32,
) -> u32 {
    if sample_rate == 0 || periods.fundamental == 0 {
        return periods.default;
    }
    let target = (sample_rate as f64 * TARGET_PERIOD_MS / 1000.0).round() as u32;
    let bounded = target.clamp(periods.min, periods.max);
    let aligned = (bounded / periods.fundamental) * periods.fundamental;
    let aligned = if aligned < periods.min { periods.min } else { aligned };
    if aligned == 0 { periods.default } else { aligned }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn periods(min: u32, max: u32, default: u32, fundamental: u32) -> wasapi::SharedModeEnginePeriod {
        wasapi::SharedModeEnginePeriod { default, fundamental, min, max }
    }

    #[test]
    fn period_targets_ten_ms_and_aligns() {
        // 48 kHz : cible 480 frames ; pas de 128 → 384 (aligné vers le bas).
        let p = periods(128, 4800, 480, 128);
        assert_eq!(choose_period(p, 48_000), 384);
    }

    #[test]
    fn period_is_bounded_and_falls_back() {
        let p = periods(256, 1024, 512, 256);
        // 96 kHz → cible 960, bornée à max 1024, alignée sur 256 → 1024.
        assert_eq!(choose_period(p, 96_000), 1024);
        // Pas nul : on renvoie le défaut.
        assert_eq!(choose_period(periods(256, 1024, 512, 0), 48_000), 512);
    }
}

/// Convertit un bloc de samples f32 entrelacés en octets dans le format du mix.
fn samples_to_bytes(samples: &[f32], format: &WaveFormat) -> Result<Vec<u8>, String> {
    let sample_type = format.get_subformat().map_err(|e| format!("subformat: {e:?}"))?;
    let bits = format.get_bitspersample();
    let mut out = Vec::with_capacity(samples.len() * (bits as usize / 8).max(1));
    match (sample_type, bits) {
        (SampleType::Float, 32) => {
            for &s in samples {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        (SampleType::Int, 16) => {
            for &s in samples {
                out.extend_from_slice(&super::samples::to_i16(s).to_le_bytes());
            }
        }
        (SampleType::Int, 24) => {
            // 24 bits packed sur 3 octets (S24_3LE).
            for &s in samples {
                let v = super::samples::to_i24(s).to_le_bytes();
                out.extend_from_slice(&v[..3]);
            }
        }
        (SampleType::Int, 32) => {
            // 24 bits significatifs alignés à gauche dans un mot 32 (S24_4LE)
            // quand le format l'indique ; sinon 32 bits pleins.
            let valid = format.get_validbitspersample();
            if valid == 24 {
                for &s in samples {
                    out.extend_from_slice(&(super::samples::to_i24(s) << 8).to_le_bytes());
                }
            } else {
                for &s in samples {
                    out.extend_from_slice(&super::samples::to_i32(s).to_le_bytes());
                }
            }
        }
        (t, b) => return Err(format!("format de mix non géré : {t:?} / {b}-bit")),
    }
    Ok(out)
}

/// Infos renvoyées au thread principal dès que le flux est négocié.
#[derive(Debug, Clone, Copy)]
struct ReadyInfo {
    sample_rate: u32,
    channels: u16,
    period_frames: u32,
}

/// Ce qui est envoyé au thread de rendu après une négociation réussie.
struct RenderConfig<C> {
    consumer: C,
    atomics: PlaybackAtomics,
    shared: SymphoniaSharedState,
}

/// Initialisation WASAPI + boucle de rendu, dans un thread dédié qui possède
/// tous les objets COM. Envoie `ReadyInfo` (ou une erreur) dès que le flux est
/// prêt, puis attend le `RenderConfig` avant de démarrer la lecture.
fn thread_main<C>(
    device_name: String,
    ready_tx: mpsc::Sender<Result<ReadyInfo, String>>,
    config_rx: mpsc::Receiver<RenderConfig<C>>,
    stop: Arc<AtomicBool>,
) where
    C: Consumer<Item = f32> + Send + 'static,
{
    let result = (|| -> Result<ReadyInfo, String> {
        let hr = initialize_mta();
        if !hr.is_ok() {
            return Err(format!("COM MTA init: HRESULT 0x{:08X}", hr.0));
        }

        let enumerator = DeviceEnumerator::new()
            .map_err(|e| format!("DeviceEnumerator: {e:?}"))?;
        let device = resolve_wasapi_device(&enumerator, Some(&device_name))?;
        let mut audio_client = device
            .get_iaudioclient()
            .map_err(|e| format!("get_iaudioclient: {e:?}"))?;

        // Le format du mix est le seul garanti en mode partagé.
        let mix_format = audio_client
            .get_mixformat()
            .map_err(|e| format!("get_mixformat: {e:?}"))?;
        let sample_rate = mix_format.get_samplespersec();
        let channels = mix_format.get_nchannels();

        let periods = audio_client
            .get_shared_mode_engine_period(&mix_format)
            .map_err(|e| format!("IAudioClient3 non disponible : {e:?}"))?;
        let period_frames = choose_period(periods, sample_rate);

        audio_client
            .initialize_shared_audio_stream(&mix_format, TimingMode::Events, period_frames)
            .map_err(|e| format!("initialize_shared_audio_stream: {e:?}"))?;

        let event = audio_client
            .set_get_eventhandle()
            .map_err(|e| format!("set_get_eventhandle: {e:?}"))?;
        let render_client = audio_client
            .get_audiorenderclient()
            .map_err(|e| format!("get_audiorenderclient: {e:?}"))?;

        let info = ReadyInfo { sample_rate, channels, period_frames };
        if ready_tx.send(Ok(info)).is_err() {
            // Le thread principal a renoncé (timeout) : on s'arrête là.
            return Ok(info);
        }

        let config = config_rx.recv().map_err(|e| format!("config never arrived: {e}"))?;
        let render = super::cpal_symphonia::render_fn(
            config.consumer,
            config.atomics,
            config.shared,
            super::samples::MARGE_PARTAGEE,
            sample_rate,
        );
        render_loop(
            audio_client,
            event,
            render_client,
            mix_format,
            render,
            stop,
        )
        .map_err(|e| format!("render_loop: {e}"))?;
        Ok(info)
    })();

    if let Err(e) = result {
        // Le thread principal n'attend plus le ready en cas d'erreur de rendu,
        // mais le signaler reste utile pour le diagnostic.
        let _ = ready_tx.send(Err(e.clone()));
        log::error!("❌ WASAPI low-latency : {e}");
    }
}

/// Boucle de rendu IAudioClient3 event-driven. Tous les objets restent dans ce
/// thread (ils ne sont pas `Send`).
#[allow(clippy::too_many_arguments)]
fn render_loop<R>(
    audio_client: wasapi::AudioClient,
    event: wasapi::Handle,
    render_client: wasapi::AudioRenderClient,
    mix_format: WaveFormat,
    mut render: R,
    stop: Arc<AtomicBool>,
) -> Result<(), String>
where
    R: FnMut(&mut [f32]) + Send,
{
    let channels = mix_format.get_nchannels() as usize;
    let bytes_per_frame = mix_format.get_blockalign() as usize;
    let mut f32_buffer: Vec<f32> = Vec::new();

    // Remplir le tampon de silence AVANT start_stream : en event-driven le
    // contenu initial est indéfini et sortirait sous forme de clic.
    if let Ok(frames) = audio_client.get_available_space_in_frames() {
        let silence = vec![0u8; frames as usize * bytes_per_frame];
        let _ = render_client.write_to_device(&silence, None);
    }
    audio_client
        .start_stream()
        .map_err(|e| format!("start_stream: {e:?}"))?;

    log::info!(
        "▶️  WASAPI low-latency (IAudioClient3) : flux démarré ({} Hz, {} ch, buffer {} frames)",
        mix_format.get_samplespersec(),
        channels,
        audio_client.get_buffer_size().unwrap_or(0)
    );

    while !stop.load(Ordering::Relaxed) {
        if event.wait_for_event(100).is_err() {
            continue;
        }
        let available = match audio_client.get_available_space_in_frames() {
            Ok(n) => n as usize,
            Err(e) => {
                log::warn!("get_available_space_in_frames: {e:?}");
                continue;
            }
        };
        if available == 0 {
            continue;
        }
        f32_buffer.resize(available * channels, 0.0);
        render(&mut f32_buffer);
        let out_bytes = samples_to_bytes(&f32_buffer, &mix_format)?;
        if let Err(e) = render_client.write_to_device(&out_bytes, None) {
            log::warn!("write_to_device: {e:?}");
        }
    }

    if let Err(e) = audio_client.stop_stream() {
        log::warn!("stop_stream: {e:?}");
    }
    log::info!("⏹  WASAPI low-latency : flux arrêté");
    Ok(())
}

/// Backend IAudioClient3 shared low-period pour la voie Symphonia.
pub struct WasapiLowLatencyOutput {
    device_name: String,
    sample_rate: u32,
    channels: u16,
    period_frames: u32,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Erreur de construction emportant le `Consumer` pour permettre le repli CPAL.
/// `None` seulement si le thread a déjà reçu le Consumer avant d'échouer.
pub struct WasapiLowLatencyBuildError<C> {
    pub consumer: Option<C>,
    pub error: AudioOutputError,
}

impl WasapiLowLatencyOutput {
    /// Tente d'ouvrir un flux partagé IAudioClient3 à période réduite.
    ///
    /// Le `Consumer` reste côté appelant jusqu'à ce que la négociation réussisse
    /// (signal `ReadyInfo`) : en cas d'échec il est rendu pour le repli CPAL.
    pub fn try_new<C>(
        device_name: String,
        consumer: C,
        atomics: PlaybackAtomics,
        shared: SymphoniaSharedState,
    ) -> Result<Self, WasapiLowLatencyBuildError<C>>
    where
        C: Consumer<Item = f32> + Send + 'static,
    {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<ReadyInfo, String>>();
        let (config_tx, config_rx) = mpsc::channel::<RenderConfig<C>>();
        let stop = Arc::new(AtomicBool::new(false));

        let thread_stop = stop.clone();
        let thread_device = device_name.clone();
        let thread = std::thread::Builder::new()
            .name("rustmusic-wasapi-lowlatency".into())
            .spawn(move || thread_main(thread_device, ready_tx, config_rx, thread_stop))
            .map_err(|e| AudioOutputError::BuildFailed(format!("spawn render thread: {e}")));

        let thread = match thread {
            Ok(t) => t,
            Err(error) => {
                return Err(WasapiLowLatencyBuildError { consumer: Some(consumer), error })
            }
        };

        let ready = ready_rx.recv_timeout(READY_TIMEOUT);
        let info = match ready {
            Ok(Ok(info)) => info,
            Ok(Err(e)) => {
                stop.store(true, Ordering::SeqCst);
                // Libère le receiver du thread s'il attendait le Consumer.
                drop(config_tx);
                let _ = thread.join();
                return Err(WasapiLowLatencyBuildError {
                    consumer: Some(consumer),
                    error: AudioOutputError::NegotiationFailed(e),
                });
            }
            Err(e) => {
                stop.store(true, Ordering::SeqCst);
                drop(config_tx);
                let _ = thread.join();
                let reason = match e {
                    mpsc::RecvTimeoutError::Timeout => "négociation IAudioClient3 expirée".into(),
                    mpsc::RecvTimeoutError::Disconnected => "thread de rendu terminé".into(),
                };
                return Err(WasapiLowLatencyBuildError {
                    consumer: Some(consumer),
                    error: AudioOutputError::NegotiationFailed(reason),
                });
            }
        };

        if config_tx
            .send(RenderConfig { consumer, atomics, shared })
            .is_err()
        {
            stop.store(true, Ordering::SeqCst);
            let _ = thread.join();
            return Err(WasapiLowLatencyBuildError {
                consumer: None,
                error: AudioOutputError::NegotiationFailed("thread de rendu disparu".into()),
            });
        }

        let began = Instant::now();
        log::debug!(
            "▶️  WASAPI low-latency : backend prêt en {} ms ({} Hz, période {} frames)",
            began.elapsed().as_millis(),
            info.sample_rate,
            info.period_frames
        );

        Ok(Self {
            device_name,
            sample_rate: info.sample_rate,
            channels: info.channels,
            period_frames: info.period_frames,
            stop,
            thread: Some(thread),
        })
    }
}

impl AudioOutput for WasapiLowLatencyOutput {
    fn start(&mut self) -> Result<(), AudioOutputError> {
        // Le flux démarre dans le thread de rendu dès réception du Consumer.
        Ok(())
    }

    fn output_sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn output_channels(&self) -> u16 {
        self.channels
    }

    fn device_name(&self) -> &str {
        &self.device_name
    }

    fn backend(&self) -> AudioBackend {
        AudioBackend::WasapiLowLatency
    }
}

impl Drop for WasapiLowLatencyOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                log::warn!("WASAPI low-latency : le thread de rendu a paniqué");
            }
        }
        // Laisse le temps au flux de rendre l'endpoint proprement.
        std::thread::sleep(Duration::from_millis(20));
    }
}
