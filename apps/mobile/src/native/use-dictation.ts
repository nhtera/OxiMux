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
import { useCallback, useState } from 'react';

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

type Options = {
  /** Called with a non-empty transcript to insert into the composer. */
  onText: (text: string) => void;
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

export function useDictation({ onText, onError }: Options) {
  const client = useClient((s) => s.client);
  const recorder = useAudioRecorder(RECORDING_OPTIONS);
  // Poll often enough that the level meter reads as live, not stepped.
  const recorderState = useAudioRecorderState(recorder, 100);
  const [phase, setPhase] = useState<DictationPhase>('idle');

  // Dictation needs the desktop to decode, so the button only exists while
  // connected — a disconnected phone hides it rather than offering a dead action.
  const available = !!client;

  const start = useCallback(async () => {
    if (!client || phase === 'recording' || phase === 'transcribing') return;
    try {
      let permission = await getRecordingPermissionsAsync();
      if (!permission.granted && permission.canAskAgain) {
        permission = await requestRecordingPermissionsAsync();
      }
      if (!permission.granted) {
        setPhase('denied');
        onError('Microphone access is off — enable it in Settings to dictate.');
        return;
      }
      await setAudioModeAsync({ allowsRecording: true, playsInSilentMode: true });
      await recorder.prepareToRecordAsync(RECORDING_OPTIONS);
      recorder.record();
      setPhase('recording');
    } catch (e) {
      setPhase('idle');
      onError(describeError(e));
    }
  }, [client, phase, recorder, onError]);

  const stop = useCallback(async () => {
    if (phase !== 'recording') return;
    setPhase('transcribing');
    try {
      await recorder.stop();
      const uri = recorder.uri;
      if (!uri) throw new Error('No recording was captured.');
      const audioBase64 = await readClipBase64(uri);
      const text = await client!.transcribeAudio(audioBase64, SAMPLE_RATE);
      // An empty transcript is a silent clip — insert nothing rather than a blank.
      if (text.trim().length > 0) onText(text);
    } catch (e) {
      onError(describeError(e));
    } finally {
      setPhase('idle');
    }
  }, [phase, recorder, client, onText, onError]);

  const cancel = useCallback(async () => {
    if (phase === 'recording') {
      try {
        await recorder.stop();
      } catch {
        // Discarding — a failed stop on a cancel is not worth surfacing.
      }
    }
    setPhase('idle');
  }, [phase, recorder]);

  // Auto-stop at the cap so a forgotten recording does not run past what the
  // host will accept; the button also shows the elapsed time approaching it.
  if (phase === 'recording' && recorderState.durationMillis >= MAX_RECORDING_MS) {
    void stop();
  }

  const level = phase === 'recording' ? meteringToLevel(recorderState.metering) : 0;

  return { phase, level, available, start, stop, cancel };
}
