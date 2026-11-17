# v86-server
Four pieces, nothing else at the top. Only `contracts/` is shared.
- contracts/ - the shared language (schema, core, instructions, events, screens, stores, testkit)
- app/ - all business (wiring, deploy, core kernel, plugins)
- mixes/ - everything people see (mm_ussd, mm_sms, packs)
- gates/ - every connection (gm_rest, gm_momo, gm_db, gm_rust, gm_signer ...)
Rule: no piece imports another piece, only contracts.
