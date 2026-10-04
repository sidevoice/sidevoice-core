import createClient from "openapi-fetch";
import type { components, paths } from "./generated/core-http.js";

// This proof client does not select a production host, credential, or transport.
export function createNodeClient(baseUrl: string, fetchImpl: typeof fetch) {
  return createClient<paths>({ baseUrl, fetch: fetchImpl });
}

export type VoiceMediaCommand = components["schemas"]["VoiceMediaCommand"];

export function sendVoiceMedia(send: (frame: string) => void, command: VoiceMediaCommand) {
  send(JSON.stringify(command));
}
