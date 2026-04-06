// Forcing function: add an RPC to admin.proto → regenerate → this test
// fails until adminMock grows the matching stub. Prevents the "page
// renders child that calls unstubbed RPC → `admin.X is not a function`
// at render" gotcha from recurring silently each time the proto grows.
//
// connect-es v2's GenService exposes `.method` (singular) — a map keyed
// by the camelCase localName, mirroring the client surface that
// createClient() returns. That's exactly the key-set adminMock must
// cover.
import { describe, expect, it } from 'vitest';
import { AdminService } from '../gen/admin_pb';
import { adminMock } from './admin-mock';

describe('adminMock surface parity', () => {
  it('stubs every AdminService method', () => {
    const protoMethods = Object.keys(AdminService.method);
    expect(protoMethods.length).toBeGreaterThan(0);
    for (const m of protoMethods) {
      expect(adminMock, `missing stub for AdminService.${m}`).toHaveProperty(m);
    }
  });

  it('has no stray stubs outside AdminService', () => {
    // Catches typos (e.g. `triggerGc` vs the proto's `triggerGC`). A
    // stray key is a stub nobody consumes — dead weight and a masked
    // misspelling of the intended method.
    const protoMethods = new Set(Object.keys(AdminService.method));
    for (const k of Object.keys(adminMock)) {
      expect(protoMethods.has(k), `stray stub ${k} not in AdminService`).toBe(true);
    }
  });
});
