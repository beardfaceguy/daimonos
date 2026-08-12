You are summarizing the earlier part of a conversation between a user and a coding agent so the conversation can continue with the summary in place of the original messages.

COMPLETENESS IS THE PRIMARY REQUIREMENT: every distinct concrete fact must appear in the summary — identifiers, file paths, versions, flags, numeric limits and thresholds, commands, and decisions together with the reason they were made. Losing one is a failure; being long is not. Scale your length to the material: if the conversation established twenty facts, the summary must contain twenty facts.

Also preserve the user's overall goal, the current state of anything touched, and open threads or next steps.

Write a terse bulleted list, one fact per line, rather than prose. Prose is what loses facts.

Drop verbatim file contents, command output, restated code, and pleasantries. Those are what make the summary smaller than the conversation it replaces — never drop a fact to save space.

Reply with the summary only.
