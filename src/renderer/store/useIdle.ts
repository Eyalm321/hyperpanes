import { create } from 'zustand';
import { useSettings } from './useSettings';
import type { Pane } from '../types';

// Per-pane "AI is idle" tracking. The signal is pure output quiescence: every
// byte a pane's pty emits calls markActivity (from Terminal's onData), which
// resets a timer. If the timer ever elapses — no output for idleAlertSeconds —
// the pane is flagged idle, which the frame turns into a soft firefly glow.
//
// This is deliberately a heuristic. A "thinking" spinner emits bytes the whole
// time it runs, so a pane only goes quiet once the agent has actually finished
// and is sitting at its prompt waiting for you — which is exactly when we want
// to catch your eye. (For a truly authoritative signal we'd wire Claude Code's
// Stop/Notification hooks; quiescence is the zero-config layer.)

// Names that mark a pane as an AI/agent CLI worth watching. Matched against the
// pane's command, label, subtitle and live shell title, so it catches both
// `claude`-launched panes and a plain shell whose title becomes "claude".
const AI_PATTERN =
  /(^|[\s\\/])(claude|aider|gemini|ollama|llm|chatgpt|codex|cursor-agent|goose|cody|copilot|continue)\b/i;

export function isAiPane(pane: Pane, shellTitle = ''): boolean {
  const hay = `${pane.command ?? ''} ${pane.label} ${pane.subtitle ?? ''} ${shellTitle}`;
  return AI_PATTERN.test(hay);
}

interface IdleState {
  // paneId -> true once it has been quiet past the threshold.
  idle: Record<string, boolean>;
  // Record output activity for a pane: clear any idle flag and (re)arm the timer.
  markActivity: (paneId: string) => void;
  // Drop a pane from tracking (exit / unmount): cancel its timer, clear flag.
  forget: (paneId: string) => void;
}

// Timers live outside the store — they're imperative plumbing, not state.
const timers = new Map<string, ReturnType<typeof setTimeout>>();

export const useIdle = create<IdleState>((set, get) => ({
  idle: {},

  markActivity: (paneId) => {
    const s = useSettings.getState();
    // Globally off → don't track at all (frames won't glow regardless).
    if (!s.idleAlert) return;

    // Output means "not idle right now": clear the flag if it was set.
    if (get().idle[paneId]) {
      set((st) => ({ idle: { ...st.idle, [paneId]: false } }));
    }

    const prev = timers.get(paneId);
    if (prev) clearTimeout(prev);
    const t = setTimeout(() => {
      timers.delete(paneId);
      set((st) => ({ idle: { ...st.idle, [paneId]: true } }));
    }, Math.max(1, s.idleAlertSeconds) * 1000);
    timers.set(paneId, t);
  },

  forget: (paneId) => {
    const prev = timers.get(paneId);
    if (prev) {
      clearTimeout(prev);
      timers.delete(paneId);
    }
    set((st) => {
      if (!(paneId in st.idle)) return st;
      const next = { ...st.idle };
      delete next[paneId];
      return { idle: next };
    });
  }
}));
