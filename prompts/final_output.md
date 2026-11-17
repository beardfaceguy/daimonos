<!--
Model-facing corrective text for structured final output (ADR-024, WP05).
Sections are delimited by `## <key>` headings and read by
src/structured_output.rs. Edit the wording here, never in Rust string literals.
Keep each section to plain guidance; the runtime appends the concrete reason
and validation errors as data after the `retry` text.
-->

## retry
Your previous response did not produce a single valid final answer.

To finish the run, respond with exactly one call to the designated
final-answer tool and nothing else that changes the outcome. Put the complete
result in that call's arguments so it satisfies the required schema. You may
include brief reasoning text, but do not emit any other tool call alongside the
final-answer call, and do not answer only in prose.

The specific problem with the last attempt was:

## mixed_calls
The response combined the final-answer call with other tool calls, so the run
executed none of them. Send the final answer as a single call on its own, or
gather more evidence first and call the final-answer tool only when you are
ready to finish.

## no_final_call
The response contained no call to the designated final-answer tool, so the run
cannot finish. Call that tool with arguments that satisfy the schema when you
are ready to conclude.

## invalid_arguments
The final-answer call's arguments did not satisfy the required schema. Correct
the arguments shown below and call the final-answer tool once more.
