Review this inbox note and call `submit_proposal`.

## Note under review

Permalink: `{{origin_permalink}}`

```markdown
{{origin}}
```

## Other free inbox notes that may cover the same topic

You may merge any of these into the proposal by listing their permalinks in `sources`.
Only these permalinks and the note under review are allowed in `sources`.

{{candidates}}

## Related notes already verified

If the note under review only repeats one of these, propose `delete`.

{{verified}}

## Existing folders under `{{verified_dir}}/`

{{verified_dirs}}

Rules for the tool call: `sources` must contain `{{origin_permalink}}`; `promote` has exactly
one source, `merge` two or more; `target_dir` is a single folder name; `rationale` is one to
three sentences for the human reviewer, in the language of the notes.
For `promote` and `merge` include `changes` (see the system message); for `delete` leave it out.
