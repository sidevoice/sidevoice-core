import { createNodeClient, sendVoiceMedia } from "./client.js";

const node = createNodeClient("http://127.0.0.1", fetch);
node.POST("/api/presentation/languages");
node.GET("/api/unknown");
node.POST("/api/models/transcription/preview", {
  body: {
    place: "openai",
    model: "gpt-4o-transcribe",
    audio: { encoding: "pcm_s16le", sample_rate: 8000, data_base64: "" }
  }
});
sendVoiceMedia(() => {}, { type: "voice-unknown", data: { session_id: "session", path: "socket" } });
