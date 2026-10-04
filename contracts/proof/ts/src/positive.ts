import type { components } from "./generated/core-http.js";
import { createNodeClient, sendVoiceMedia } from "./client.js";

export async function representativeCalls(baseUrl: string, fetchImpl: typeof fetch) {
  const node = createNodeClient(baseUrl, fetchImpl);
  const defaults = await node.GET("/api/presentation/languages");
  if (defaults.data) {
    const requiredNull: null = defaults.data.stt.build;
    const originalInteger: number = defaults.data.replay_on_return_seconds;
    const decimal: number = defaults.data.smart_turn_min_silence;
    void [requiredNull, originalInteger, decimal];
  }
  const trial = await node.POST("/api/models/transcription/preview", {
    body: {
      place: "openai",
      model: "gpt-4o-transcribe",
      audio: { encoding: "pcm_s16le", sample_rate: 16000, data_base64: "" }
    }
  });
  if (trial.data) {
    const text: string = trial.data.text;
    void text;
  }
  const optionalOptions: components["schemas"]["TrialRequest"] = {
    place: "openai",
    model: "gpt-4o-transcribe",
    audio: { encoding: "pcm_s16le", sample_rate: 16000, data_base64: "" }
  };
  const refusal: components["schemas"]["TrialRefusal"] = {
    detail: { key: "trial.silent", message: "localized in the actual response" }
  };
  sendVoiceMedia(() => {}, {
    type: "voice-media",
    data: { session_id: "session", path: "socket" }
  });
  return { optionalOptions, refusal };
}
