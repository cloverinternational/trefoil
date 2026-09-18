import { describe, expect, it } from 'vitest';
import { parseSseChunk, systemLogReconnectDelay } from './systemLogs';

describe('system log SSE parsing', () => {
  it('parses events split across reads and handles CRLF and multiline data', () => {
    const first = parseSseChunk('', 'event: syslog\r\ndata: {"message":"hel');
    expect(first.events).toEqual([]);
    const second = parseSseChunk(first.remainder, 'lo"}\r\ndata: second\r\n\r\nevent: ping\r\ndata: ignored\r\n\r\n');
    expect(second.events).toEqual([
      { event: 'syslog', data: '{"message":"hello"}\nsecond' },
      { event: 'ping', data: 'ignored' },
    ]);
    expect(second.remainder).toBe('');
  });

  it('ignores comments and records without data while retaining incomplete records', () => {
    const parsed = parseSseChunk('', ': heartbeat\n\nevent: syslog\ndata: {"message":"pending"}');
    expect(parsed.events).toEqual([]);
    expect(parsed.remainder).toBe('event: syslog\ndata: {"message":"pending"}');
  });
});

describe('system log reconnect scheduling', () => {
  it('uses deterministic capped exponential backoff', () => {
    expect([0, 1, 2, 5, 10].map(systemLogReconnectDelay)).toEqual([1000, 2000, 4000, 30000, 30000]);
  });
});
