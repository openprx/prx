// `after_seq` 0 means "from the start". `next_seq` is the last seq returned, so
// a cursor of N whose oldest retained row is N + 1 has not missed a row. A row
// was dropped only when the oldest retained seq is at least N + 2.
export function resultsWereTrimmed(requested, oldestSeq) {
  return Number.isFinite(requested)
    && Number.isFinite(oldestSeq)
    && requested > 0
    && requested + 1 < oldestSeq;
}
