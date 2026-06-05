import { Session, type SpawnOptions } from './session';

export interface SessionHandlers {
  onData: (uid: string, data: string) => void;
  onExit: (uid: string, code: number) => void;
}

/** Owns every live pty Session, keyed by uid. */
export class SessionManager {
  private sessions = new Map<string, Session>();

  get(uid: string): Session | undefined {
    return this.sessions.get(uid);
  }

  create(opts: SpawnOptions, handlers: SessionHandlers): Session {
    const session = new Session(opts);
    session.on('data', (data: string) => handlers.onData(opts.uid, data));
    session.on('exit', (code: number) => {
      handlers.onExit(opts.uid, code);
      this.sessions.delete(opts.uid);
    });
    this.sessions.set(opts.uid, session);
    return session;
  }

  write(uid: string, data: string) {
    this.sessions.get(uid)?.write(data);
  }

  resize(uid: string, cols: number, rows: number) {
    this.sessions.get(uid)?.resize(cols, rows);
  }

  kill(uid: string) {
    const session = this.sessions.get(uid);
    if (session) {
      session.destroy();
      this.sessions.delete(uid);
    }
  }

  killAll() {
    for (const session of this.sessions.values()) session.destroy();
    this.sessions.clear();
  }
}
