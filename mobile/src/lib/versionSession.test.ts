import { describe, expect, it } from 'vitest';
import { createVersionSession, SESSION_IDLE_MS } from './versionSession';

type Step =
  | ['snapshot', number, boolean]
  | ['result', boolean]
  | ['noteSave', number]
  | ['markTaken', number];

const T0 = 1_800_000_000_000;

// The session rules match the desktop editor: at most one version per
// session, a session ends after 10 min idle, and a blank stored doc (null)
// leaves the session open so the next save tries again.
describe('createVersionSession', () => {
  it.each<[string, Step[]]>([
    [
      'the first save snapshots, the second does not',
      [
        ['snapshot', T0, true],
        ['noteSave', T0],
        ['snapshot', T0 + 1_000, false],
      ],
    ],
    [
      'more than 10 min idle after a taken snapshot starts a new session',
      [
        ['snapshot', T0, true],
        ['noteSave', T0],
        ['snapshot', T0 + SESSION_IDLE_MS + 1, true],
        ['noteSave', T0 + SESSION_IDLE_MS + 1],
        ['snapshot', T0 + SESSION_IDLE_MS + 2, false],
      ],
    ],
    [
      'exactly 10 min idle stays in the session',
      [
        ['snapshot', T0, true],
        ['noteSave', T0],
        ['snapshot', T0 + SESSION_IDLE_MS, false],
      ],
    ],
    [
      'a null result leaves the session open',
      [
        ['snapshot', T0, true],
        ['result', false],
        ['noteSave', T0],
        ['snapshot', T0 + 1_000, true],
        ['result', true],
        ['noteSave', T0 + 1_000],
        ['snapshot', T0 + 2_000, false],
      ],
    ],
    [
      'markTaken suppresses the next snapshot',
      [
        ['markTaken', T0],
        ['snapshot', T0, false],
        ['noteSave', T0],
        ['snapshot', T0 + 1_000, false],
      ],
    ],
    [
      'markTaken after a long idle restarts the idle gap',
      [
        ['snapshot', T0, true],
        ['noteSave', T0],
        ['markTaken', T0 + SESSION_IDLE_MS + 1],
        ['snapshot', T0 + SESSION_IDLE_MS + 2, false],
      ],
    ],
  ])('%s', (_, steps) => {
    const session = createVersionSession();
    for (const step of steps) {
      if (step[0] === 'snapshot') expect(session.shouldSnapshot(step[1])).toBe(step[2]);
      else if (step[0] === 'result') session.result(step[1]);
      else if (step[0] === 'noteSave') session.noteSave(step[1]);
      else session.markTaken(step[1]);
    }
  });
});
