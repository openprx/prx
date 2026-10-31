// `after_seq` 0 means "from the start". A retained window that begins at 1 is
// a fresh log, not a gap. A later cursor is missed only when it is older than
// the oldest row the gateway still has.
export function resultsWereTrimmed(requested, oldestSeq) {
  return Number.isFinite(requested) && Number.isFinite(oldestSeq) && requested > 0 && requested < oldestSeq;
}
