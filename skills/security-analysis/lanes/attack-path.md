# Lane: attack-path-reviewer — chaining (Wave 2)

Use the red-team skill; if the preamble lists it as unavailable, chain the findings
directly from this brief. The Wave 1 lanes already found the individual bugs; your job is to
chain them.

1. Take the findings above as a starting inventory of attacker capabilities.
2. Build the attack paths: which findings compose? A Low-severity information disclosure
   that supplies the precondition for a Medium-severity auth weakness is a High-severity
   chain, and no single-issue lane can see it.
3. For each chain, give the entry point, the ordered steps, the findings it uses, the
   end state, and the severity of the chain as a whole.
4. Identify choke points: the single fix that breaks the most chains. Call these out
   explicitly — they are the highest-value remediations in the report.
5. Flag any capability a chain needs that no lane reported. That gap is itself a finding:
   either the capability exists and was missed, or the chain is broken. Say which.

Report chains using the standard finding format, with the component findings listed in
the Attack field.
