# r2-2026-10-22 evidence

Reproduce with a clean checkout of the reviewed head (`3ad6fba`).

1. `probe-head.log` / `probe-prefix-0ec8096.log`
   - Source: `probe_registration_parking.rs` (the interruption suite plus one probe case).
   - The probe registers a gated plugin through a module, then the host instantiates a
     second fiber for the same plugin through `RuntimeAssembly::runtime().
     capability_registry().instantiate(.., "slow")`, so the registration-disposal step
     really has an await point.
   - At HEAD the parked disposal resumes, cleans the module and reports no failure.
   - At `0ec8096` (pre-`fdc8e5e`) the same sequence reports
     "the capability registry authority was released with the runtime owner ...".
   - Running it needs `capability-graph` as a dev-dependency of runtime-composition
     (temporarily) because `CapabilityContext` is not re-exported.

2. `closure-probe/`
   - A standalone crate that depends only on `runtime-loader` and names the types the
     host receives on supported paths. `check.log` shows nine E0425 errors, i.e. the
     "closure" claim is not met for those types at this head.

Both probes are throwaway: neither is part of the workspace, and the review changed no
tracked file except this evidence directory and the review record.
