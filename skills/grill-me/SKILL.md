---
name: grill-me
description: Grill the user relentlessly about a plan, decision, or idea, using selectable multiple-choice answers. Use when the user wants to stress-test their thinking, or uses any 'grill' trigger phrases.
---

Interview the user relentlessly until you reach a shared understanding. Map this as a **design tree**: every decision branches into the decisions that hang off it.

Work the tree in **rounds**. The **frontier** is every decision whose prerequisites are already settled: the questions you can ask _now_ without guessing at answers you haven't heard yet. Ask the frontier in one round, then wait for the user's answers before the next round.

## Asking questions

Ask every question with the **AskUserQuestion** tool so the user can pick answers instead of typing them. Do not print questions as plain text.

- One tool call holds at most **4 questions**. If the frontier is bigger, ask the 4 most foundational now and the rest in a follow-up call in the same round.
- Each question gets **2-4 distinct options**. Don't add an "Other" option: the tool adds one automatically for free-text answers.
- Put your recommended answer **first** and end its label with " (Recommended)".
- Keep `label` short (1-5 words). Use `description` for the trade-off or consequence of picking it.
- `header` is a short chip (max 12 chars) naming the topic, e.g. "Storage", "Auth", "Scope".
- Use `multiSelect: true` only when the choices really aren't mutually exclusive.
- Use `preview` when the options are concrete things the user should compare side by side (code snippets, schemas, layouts, config).
- If a decision really is open-ended and can't be reduced to options, still offer your 2-3 best candidates. The user can pick "Other" and type.

Before the tool call, you may write a short line of context (what this round covers, or any facts you found), but keep the questions themselves in the tool.

## Working the tree

Each round the user answers reshapes the tree: settled decisions push the frontier outward and unblock questions that depended on them. Recompute the frontier and ask the next round. A question whose answer depends on another question still open in this round belongs to a _later_ round, not this one.

Finding _facts_ is your job, never the user's. When a frontier question needs a fact from the environment (filesystem, tools, etc.), dispatch a sub-agent to find it; don't ask the user for anything you could look up yourself. Don't block on it: a running exploration is an unsettled prerequisite, so only the questions downstream of it wait for the sub-agent to report; ask the rest of the frontier now. The _decisions_ are the user's: put each to them and wait.

The session is done when the frontier is empty: every branch of the design tree visited, nothing left silently assumed. Summarize the settled decisions, then use AskUserQuestion once more to confirm you have reached a shared understanding (e.g. "Looks right, proceed" / "Revisit a decision"). Do not act on it until the user confirms.
