The human reviewer commented on your proposal. Answer and, if the comments ask for it, revise
the proposal. Call `submit_reply`.

## Current proposal (draft version {{version}})

- action: `{{action}}`
- sources: {{sources_list}}
- target: `{{target}}`
- tags: {{tags}}
- rationale: {{rationale}}

```markdown
{{draft}}
```

## Source notes as they are now

{{sources}}

## Other free inbox notes you may add as sources

{{candidates}}

## Conversation so far

{{thread}}

## New comments to answer

{{pending}}

Rules for the tool call: `reply` is a short answer to the reviewer in their language. Include
`action`, `sources`, `target_dir`, `target_title`, `draft`, `tags` or `rationale` only when you
change them (update `rationale` whenever the old one no longer describes the proposal);
omitted fields keep their current values. When you change `draft`, send the whole new draft.
