# Lane: crypto-reviewer — cryptography, consensus, and untrusted input

Review the code in scope for cryptographic, consensus, and untrusted-input defects.
Generic OWASP scanning misses these, and they are usually Critical when present.

## Randomness and key material

- Key, nonce, and salt material comes from a CSPRNG (OsRng / getrandom) — flag
  thread_rng-style convenience wrappers, seeded RNGs, and time-derived seeds.
- Secrets are zeroized on drop. Flag any that appear in Debug/Display impls, serde output,
  error messages, or logs.

## Primitive misuse

- Nonce or IV reuse across messages under the same key; counter reuse after restart.
- Secrets, MACs, and auth tags compared in constant time — flag `==` on secret bytes.
- Missing domain separation when hashing structurally different inputs into one function.
- Signature malleability; signatures that leave a security-relevant field uncovered;
  verification results that are computed but never checked.
- Home-rolled crypto where a vetted primitive exists.

## Arithmetic

- Overflow and underflow in amount, balance, fee, weight, and index arithmetic. In Rust,
  release builds wrap silently — flag bare +, -, * on value-carrying types and expect
  checked_/saturating_ with the saturation case justified.
- Precision loss from float or lossy integer casts anywhere value is computed.
- Division or modulo by a value an attacker can drive to zero.

## Denial of service from untrusted input

- Panic-as-DoS: unwrap, expect, direct indexing, slicing, and integer division applied to
  attacker-controlled data on a network or validation path. In a node, a reachable panic
  is a remote crash.
- Unbounded allocation: `with_capacity` or `resize` driven by an attacker-supplied length
  field; `collect` over an untrusted iterator; missing maximum message and field sizes.
- Deserialization without length caps or recursion-depth limits; decompression bombs.
- Unbounded work: loops, hashing, or signature verification whose iteration count comes
  from the input.

## Protocol and consensus

- Replay protection: nonces, timestamps, chain/network IDs, and domain tags are included
  in signed and authenticated payloads.
- Non-determinism on a consensus path: HashMap/HashSet iteration order, float arithmetic,
  system time, thread scheduling, or locale-dependent behaviour.
- Trust in peer-supplied timestamps, heights, or difficulty without bounds.
- Validation order: the cheap authenticity check runs before anything expensive.

Report back all findings.
