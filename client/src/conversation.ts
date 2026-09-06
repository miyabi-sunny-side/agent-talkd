import type { Message } from "./api";

// Native IDs are byte positions. Never build history from repeated snapshots.
// Stop growing the window before dropping anything the person may be reading.
export function appendMessages(
  current: Message[],
  incoming: Message[],
): Message[] | null {
  const seen = new Set(current.map((message) => message.id));
  const added = incoming.filter((message) => {
    if (seen.has(message.id)) return false;
    seen.add(message.id);
    return true;
  });
  const combined = [...current, ...added];
  if (
    combined.length > 1000 ||
    combined.reduce((size, message) => size + message.text.length * 2, 0) >
      4 * 1024 * 1024
  )
    return null;
  return combined;
}
