import { describe, expect, it } from 'vitest';
import { MessageInbox, MAX_PER_PANE } from './control-inbox';

describe('MessageInbox', () => {
  it('delivers durably and reads by cursor (at-least-once)', () => {
    const inbox = new MessageInbox();
    const m1 = inbox.post('p1', 'mgr', 'do X', 1000);
    const m2 = inbox.post('p1', 'mgr', 'do Y', 1001);
    inbox.post('p2', 'mgr', 'other', 1002); // different pane

    expect(m1.seq).toBe(1);
    expect(m2.seq).toBe(2);
    // Full read for p1, ordered by seq.
    expect(inbox.read('p1').map((m) => m.body)).toEqual(['do X', 'do Y']);
    // Cursor read: only messages after seq 1.
    expect(inbox.read('p1', m1.seq).map((m) => m.body)).toEqual(['do Y']);
    // A pane with no messages reads empty.
    expect(inbox.read('p3')).toEqual([]);
  });

  it('seq is global and monotonic across panes', () => {
    const inbox = new MessageInbox();
    inbox.post('a', 'x', '1', 0);
    const b = inbox.post('b', 'x', '2', 0);
    expect(b.seq).toBe(2);
    expect(inbox.latestSeq('a')).toBe(1);
    expect(inbox.latestSeq('b')).toBe(2);
    expect(inbox.latestSeq('missing')).toBe(0);
  });

  it('bounds per-pane history and counts the evicted overflow', () => {
    const inbox = new MessageInbox();
    for (let i = 0; i < MAX_PER_PANE + 5; i++) inbox.post('p', 'x', `m${i}`, i);
    const kept = inbox.read('p');
    expect(kept).toHaveLength(MAX_PER_PANE);
    expect(kept[0].body).toBe('m5'); // first 5 evicted
    expect(inbox.droppedCount('p')).toBe(5);
  });

  it('drop forgets a pane inbox + its dropped counter', () => {
    const inbox = new MessageInbox();
    inbox.post('p', 'x', 'hi', 0);
    inbox.drop('p');
    expect(inbox.read('p')).toEqual([]);
    expect(inbox.droppedCount('p')).toBe(0);
  });
});
