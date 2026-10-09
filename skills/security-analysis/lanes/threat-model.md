# Lane: secskill-reviewer — threat model

Use the senior-security skill to STRIDE threat-model the code in scope. If the preamble lists
it as unavailable, do the STRIDE pass and DREAD scoring directly from this brief.

Build a data-flow view of the in-scope code, identify the trust boundaries it crosses,
enumerate threats per STRIDE category (Spoofing, Tampering, Repudiation, Information
disclosure, Denial of service, Elevation of privilege), and DREAD-score each one.

Stay at the design level: report architectural and trust-boundary threats. The other lanes
cover implementation bugs.

Report back all findings.
