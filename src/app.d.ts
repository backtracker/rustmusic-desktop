declare global {
  // Version de l'app injectée par Vite (voir `define` dans vite.config.js).
  const __APP_VERSION__: string;
  // Versions majeures.mineures des crates audio, lues dans src-tauri/Cargo.toml.
  const __AUDIO_DEPS__: Record<"symphonia" | "cpal" | "rubato" | "wasapi", string>;
}

export {};
