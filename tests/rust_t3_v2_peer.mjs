// Drive the pinned connector's actual Socket.IO link against a disposable Core.
import readline from 'node:readline';
import { pathToFileURL } from 'node:url';

const [source, origin, connector_id, token] = process.argv.slice(2);
if (!source || !origin || !connector_id || !token) throw Error('Pinned peer arguments are required');
const { roomLink } = await import(pathToFileURL(source));
const print = value => process.stdout.write(JSON.stringify(value) + '\n');
let binding = null;
let deliveryDelay = 0;
let hostError = false;
const link = roomLink({ origin, connector_id, token, protocol: 2,
  identity: { host: 't3-js-peer', platform: 'linux', version: 'pinned-v2', harnesses: ['fixture'] },
  onConnected: async welcome => {
    print({ event: 'welcome', welcome });
    try {
      binding = await link.request('binding.register', {
        client_ref: 't3-js', harness: 'fixture', thread: 't3-js-thread', title: 'JS conversation',
        capabilities: { deliver: 'supported', working: 'supported' }, inbound: { ok: true }
      });
      print({ event: 'binding', binding });
    } catch (error) { print({ event: 'error', message: String(error) }); }
  },
  onLost: reason => print({ event: 'lost', reason }),
  onEvent: async (event, data) => {
    print({ event: 'from-core', method: event, data });
    if (event === 'input.deliver') {
      if (deliveryDelay) await new Promise(resolve => setTimeout(resolve, deliveryDelay));
      return { status: 'accepted', detail: 'accepted by pinned JS peer' };
    }
    if (event === 'agents.list') {
      if (hostError) return { error: { key: 'host.agent-unavailable', message: 'Bundle-derived connector message',
        params: { agent: 'fixture', raw_output: 'private output' } } };
      await new Promise(resolve => setTimeout(resolve, 1500));
      return { agents: [], custom: {}, scanned_at: null };
    }
    return {};
  }
}).open();

for await (const line of readline.createInterface({ input: process.stdin })) {
  const command = JSON.parse(line);
  if (command.op === 'publish') {
    try {
      const answer = await link.request('speech.publish', {
        event_id: command.event_id, utterance_id: command.utterance_id,
        binding_id: binding.binding_id, session_id: command.session_id,
        revision: command.revision, text: command.text, language: 'en'
      });
      print({ event: 'published', answer });
    } catch (error) { print({ event: 'error', message: String(error) }); }
  } else if (command.op === 'working') {
    link.send('input.working', { binding_id: binding.binding_id, working: command.working });
  } else if (command.op === 'read') {
    link.send('input.read', { binding_id: binding.binding_id, message_id: command.message_id });
  } else if (command.op === 'delay') {
    deliveryDelay = command.ms;
  } else if (command.op === 'host_error') {
    hostError = true;
    print({ event: 'host_error_ready' });
  } else if (command.op === 'close') {
    link.close();
    break;
  }
}
