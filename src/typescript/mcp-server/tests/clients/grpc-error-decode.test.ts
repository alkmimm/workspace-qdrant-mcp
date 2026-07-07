/**
 * Unit tests for decodePercentEncodedGrpcMessage.
 *
 * gRPC transmits the status message percent-encoded and @grpc/grpc-js surfaces
 * it without decoding, so a daemon error (e.g. an invalid regex) reaches the
 * caller with `?`→`%3F`, newlines→`%0A`, etc. These tests pin the decode that
 * runs at the grpcUnary choke point.
 */

import { describe, it, expect } from 'vitest';
import { decodePercentEncodedGrpcMessage } from '../../src/clients/daemon-client/connection.js';

describe('decodePercentEncodedGrpcMessage', () => {
  it('decodes a percent-encoded regex pattern in the message (the %3F bug)', () => {
    const err = new Error('13 INTERNAL: Search failed: regex parse error near (%3F<!%5C.)');
    const out = decodePercentEncodedGrpcMessage(err) as Error;
    expect(out).toBe(err); // mutated in place, same reference
    expect(out.message).toBe('13 INTERNAL: Search failed: regex parse error near (?<!\\.)');
    expect(out.message).not.toContain('%3F');
  });

  it('decodes percent-encoded newlines so multi-line regex errors read correctly', () => {
    const err = new Error('look-around is not supported%0A    (?<!x)%0A    ^^^');
    const out = decodePercentEncodedGrpcMessage(err) as Error;
    expect(out.message).toBe('look-around is not supported\n    (?<!x)\n    ^^^');
  });

  it('leaves a plain message untouched (no %xx escape → no-op)', () => {
    const err = new Error('5 NOT_FOUND: tenant abc has no indexed content');
    const out = decodePercentEncodedGrpcMessage(err) as Error;
    expect(out.message).toBe('5 NOT_FOUND: tenant abc has no indexed content');
  });

  it('falls back to the original string when the encoding is malformed', () => {
    const err = new Error('bad escape at 100% then %3F here');
    const out = decodePercentEncodedGrpcMessage(err) as Error;
    // `100%` is not a valid escape → decodeURIComponent throws → original kept.
    expect(out.message).toBe('bad escape at 100% then %3F here');
  });

  it('preserves the gRPC code / metadata (only display text changes)', () => {
    const err = Object.assign(new Error('13 INTERNAL: bad regex %3F'), {
      code: 13,
      details: 'bad regex %3F',
      metadata: { get: () => [] },
    });
    const out = decodePercentEncodedGrpcMessage(err) as typeof err;
    expect(out.code).toBe(13);
    expect(out.details).toBe('bad regex ?');
    expect(out.metadata).toBeDefined();
    expect(out.message).toBe('13 INTERNAL: bad regex ?');
  });

  it('returns non-Error values unchanged', () => {
    expect(decodePercentEncodedGrpcMessage('just a string')).toBe('just a string');
    expect(decodePercentEncodedGrpcMessage(undefined)).toBe(undefined);
    const obj = { message: 'x%3F' };
    expect(decodePercentEncodedGrpcMessage(obj)).toBe(obj); // not an Error → untouched
    expect(obj.message).toBe('x%3F');
  });
});
