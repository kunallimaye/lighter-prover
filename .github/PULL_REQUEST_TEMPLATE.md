## Summary

<!-- One-sentence description of the change. -->

## Reviewer checklist

- [ ] **Measurement-citation check.** For any claim that something was
  *measured / observed / profiled / benchmarked / timed*, confirm the cited
  artifact (file+row, log+line range, commit SHA+path, or named command +
  output location) **exists** and **contains the cited value**. A citation
  that cannot be resolved blocks the merge. See [Discussion #77 standing norm](https://github.com/kunallimaye/lighter-prover/discussions/77#discussioncomment-17293806).
  - On **self-review**, this check is *more* important, not skippable — note
    "I re-ran / located the artifact at `<path/row>`" in the PR record.
- [ ] Linked to a tracking issue (per repo convention).
- [ ] No fabricated or unverified measurement claims in the diff, commits, or
  PR body.
