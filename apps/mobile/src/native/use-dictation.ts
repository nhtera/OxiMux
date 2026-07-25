import {
  AudioQuality,
  IOSOutputFormat,
  getRecordingPermissionsAsync,
  requestRecordingPermissionsAsync,
  setAudioModeAsync,
  useAudioRecorder,
  useAudioRecorderState,
  type RecordingOptions,
} from 'expo-audio';
import { useCallback, useEffect, useRef, useState } from 'react';

import { toBase64 } from './base64';
import { useClient } from './client';
import { describeError } from './errors';

/**
 * The phone's voice-dictation leg.
 *
 * **The phone records; the desktop transcribes.** There is no speech engine on
 * the phone — it captures a clip and ships it to the paired desktop, which
 * decodes it with the same engine its own composer uses and returns text. So
 * this needs a live connection (unlike a note app's offline dictation), which is
 * the right trade: dictation here is a compose-time assist while paired, and the
 * desktop already owns the model, the resampling, and the filtering.
 *
 * **One round trip, not a stream.** The desktop's dictation is batch decode
 * (record fully → transcribe → final), so there is no partial-transcript UX to
 * mirror: record the whole clip, send it once, insert the one result. The clip
 * is 16 kHz mono PCM16 (a WAV), matching the engine's expected input so the
 * desktop neither resamples nor decodes a container.
 */

/** The recording sample rate. 16 kHz mono is what the speech engine expects, so
 * the desktop skips resampling entirely. */
const SAMPLE_RATE = 16_000;

/** The hard cap the desktop also enforces (`MAX_RECORDING_SECS`). Kept in sync so
 * the phone stops before the host would reject an oversized clip. */
const MAX_RECORDING_MS = 120_000;

/** How many level samples the waveform keeps. Enough to fill the bar's width on a
 * phone, so the oldest scroll off the left edge rather than the trace resetting. */
export const WAVEFORM_SAMPLES = 44;

/** How often a sample is pushed onto the waveform. Decoupled from the recorder's
 * own poll so the trace advances at a steady rate even if metering repeats. */
const WAVEFORM_INTERVAL_MS = 90;

/** 16 kHz mono PCM16 WAV: the exact shape the desktop engine decodes without a
 * resample or a container parse. iOS writes true linear PCM; Android's recorder
 * is best-effort here, the one platform where the format can drift. */
const RECORDING_OPTIONS: RecordingOptions = {
  extension: '.wav',
  sampleRate: SAMPLE_RATE,
  numberOfChannels: 1,
  bitRate: 256_000,
  isMeteringEnabled: true,
  android: {
    extension: '.wav',
    outputFormat: 'default',
    audioEncoder: 'default',
  },
  ios: {
    extension: '.wav',
    outputFormat: IOSOutputFormat.LINEARPCM,
    audioQuality: AudioQuality.HIGH,
    linearPCMBitDepth: 16,
    linearPCMIsBigEndian: false,
    linearPCMIsFloat: false,
  },
  web: {
    mimeType: 'audio/wav',
    bitsPerSecond: 256_000,
  },
};

/** Where a dictation session is in its lifecycle. `denied` is sticky until the
 * user grants access in Settings — the button shows it rather than silently
 * doing nothing. */
export type DictationPhase = 'idle' | 'recording' | 'transcribing' | 'denied';

/**
 * What the user asked for when they ended the recording.
 *
 * Two endings rather than one because dictating is where the two intents diverge
 * most: `insert` drops the transcript in the box to be read and fixed first,
 * `send` is the hands-free case where the whole point was not to touch the box.
 */
export type DictationIntent = 'insert' | 'send';

type Options = {
  /** Called with a non-empty transcript to insert into the composer. */
  onText: (text: string) => void;
  /** Called with a non-empty transcript that should be sent as a prompt. */
  onSubmit: (text: string) => void;
  /** Surfaced alongside the composer's other action failures. */
  onError: (message: string) => void;
};

/**
 * Map metering dBFS (roughly `-160..0`) to a `0..1` level for the meter. Speech
 * mostly lives in the top ~50 dB, so anchoring the floor there keeps the meter
 * lively rather than hugging zero.
 */
function meteringToLevel(metering: number | undefined): number {
  if (metering === undefined || Number.isNaN(metering)) return 0;
  const floor = -50;
  if (metering <= floor) return 0;
  if (metering >= 0) return 1;
  return (metering - floor) / -floor;
}

/** Read a recorded file URI into standard base64 for the wire. `fetch` handles
 * `file://` URIs in Expo, avoiding a separate filesystem dependency. */
async function readClipBase64(uri: string): Promise<string> {
  const res = await fetch(uri);
  const buffer = await res.arrayBuffer();
  return toBase64(new Uint8Array(buffer));
}

export function useDictation({ onText, onSubmit, onError }: Options) {
  const client = useClient((s) => s.client);
  const recorder = useAudioRecorder(RECORDING_OPTIONS);
  // Poll often enough that the level meter reads as live, not stepped.
  const recorderState = useAudioRecorderState(recorder, 100);
  const [phase, setPhase] = useState<DictationPhase>('idle');
  const [levels, setLevels] = useState<number[]>([]);
  // Set by `cancel`, read where the transcript would be handed over. Transcription
  // is a round trip to the desktop, so a cancel routinely lands while one is in
  // flight; without this the discarded clip would still arrive in the composer.
  const discarded = useRef(false);
  // The composer passes fresh closures every render (they read its current text
  // and attachments), so holding them in a ref is what keeps `start`/`stop` stable
  // enough to be safe effect dependencies — see the cap timer below.
  const handlers = useRef({ onText, onSubmit, onError });
  useEffect(() => {
    handlers.current = { onText, onSubmit, onError };
  });

  // Dictation needs the desktop to decode, so the button only exists while
  // connected — a disconnected phone hides it rather than offering a dead action.
  const available = !!client;

  const level = phase === 'recording' ? meteringToLevel(recorderState.metering) : 0;
  const latestLevel = useRef(level);
  useEffect(() => {
    latestLevel.current = level;
  }, [level]);

  // The waveform samples on its own clock rather than on the recorder's poll: a
  // steady cadence is what makes the trace *travel*, and a repeated metering
  // reading (silence, a held vowel) would otherwise stall it mid-word.
  useEffect(() => {
    if (phase !== 'recording') return;
    const id = setInterval(() => {
      setLevels((prev) => [...prev, latestLevel.current].slice(-WAVEFORM_SAMPLES));
    }, WAVEFORM_INTERVAL_MS);
    return () => clearInterval(id);
  }, [phase]);

  const start = useCallback(async () => {
    if (!client || phase === 'recording' || phase === 'transcribing') return;
    try {
      let permission = await getRecordingPermissionsAsync();
      if (!permission.granted && permission.canAskAgain) {
        permission = await requestRecordingPermissionsAsync();
      }
      if (!permission.granted) {
        setPhase('denied');
        handlers.current.onError('Microphone access is off — enable it in Settings to dictate.');
        return;
      }
      await setAudioModeAsync({ allowsRecording: true, playsInSilentMode: true });
      await recorder.prepareToRecordAsync(RECORDING_OPTIONS);
      recorder.record();
      discarded.current = false;
      setLevels([]);
      setPhase('recording');
    } catch (e) {
      setPhase('idle');
      handlers.current.onError(describeError(e));
    }
  }, [client, phase, recorder]);

  const stop = useCallback(
    async (intent: DictationIntent = 'insert') => {
      if (phase !== 'recording') return;
      setPhase('transcribing');
      try {
        await recorder.stop();
        const uri = recorder.uri;
        if (!uri) throw new Error('No recording was captured.');
        const audioBase64 = await readClipBase64(uri);
        const text = await client!.transcribeAudio(audioBase64, SAMPLE_RATE);
        if (discarded.current) return;
        // An empty transcript is a silent clip — do nothing rather than insert a
        // blank or send an empty prompt.
        if (text.trim().length === 0) return;
        if (intent === 'send') handlers.current.onSubmit(text);
        else handlers.current.onText(text);
      } catch (e) {
        if (!discarded.current) handlers.current.onError(describeError(e));
      } finally {
        setPhase('idle');
      }
    },
    [phase, recorder, client]
  );

  const cancel = useCallback(async () => {
    // Flagged before anything is awaited so an in-flight transcription sees it.
    discarded.current = true;
    if (phase === 'recording') {
      try {
        await recorder.stop();
      } catch {
        // Discarding — a failed stop on a cancel is not worth surfacing.
      }
      setPhase('idle');
    }
    // While transcribing the flag is the whole cancel: the round trip cannot be
    // recalled, so it runs to completion and its result is dropped. `stop`'s
    // `finally` returns the phase to idle.
  }, [phase, recorder]);

  // Auto-stop at the cap so a forgotten recording does not run past what the host
  // will accept. It ends as an insert, never a send: nobody asked for the prompt
  // to go out, and the clip is long enough that reading it back first matters.
  useEffect(() => {
    if (phase !== 'recording') return;
    const id = setTimeout(() => void stop('insert'), MAX_RECORDING_MS);
    return () => clearTimeout(id);
  }, [phase, stop]);

  return { phase, level, levels, available, start, stop, cancel };
}
