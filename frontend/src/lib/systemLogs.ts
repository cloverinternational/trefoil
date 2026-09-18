export type SystemLogEvent = { event: string; data: string };

/** Parse complete SSE records, retaining an incomplete record for the next read. */
export function parseSseChunk(buffer: string, chunk: string): { events: SystemLogEvent[]; remainder: string } {
  const records = `${buffer}${chunk}`.replace(/\r\n/g, '\n').replace(/\r/g, '\n').split('\n\n');
  const remainder = records.pop() ?? '';
  const events = records.flatMap((record) => {
    let event = 'message';
    const data: string[] = [];
    for (const line of record.split('\n')) {
      if (line.startsWith('event:')) event = line.slice(6).trim();
      else if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
    }
    return data.length ? [{ event, data: data.join('\n') }] : [];
  });
  return { events, remainder };
}

export const SYSTEM_LOG_RECONNECT_DELAY_MS = 1_000;

export function systemLogReconnectDelay(attempt: number): number {
  return SYSTEM_LOG_RECONNECT_DELAY_MS * Math.min(2 ** Math.max(0, attempt), 30);
}
