# Third-party notices

Trefoil is distributed under the MIT License (see `LICENSE`). It incorporates
material from the projects listed below, each under its own license.

## Plexus

- Project: <https://github.com/mcowger/plexus>
- Reference revision: `17b7e60804e3401acbe2bc2beb6b9b9042b31cce`
- License: MIT

Trefoil is a Rust port of Plexus. Code, data and documentation derived from
Plexus include:

- `backend/src/engine/` — the inference engine (request transformation,
  dispatch, routing, failover, OAuth dispatch, streaming and usage accounting),
  ported from Plexus `packages/backend/src` (`services/`, `transformers/`,
  `routes/inference/`).
- `backend/src/engine/dispatcher/oauth/cc_static_prompt.txt` — static
  system-prompt text used by the Anthropic OAuth request masking, taken from
  Plexus `transformers/oauth/`.
- `backend/src/assets/openapi.json` — the OpenAPI document bundled from Plexus
  `docs/openapi/openapi.yaml` and served at `/.well-known/plexus/openapi.json`.
- `backend/src/assets/models-ui.html` — the public models page from Plexus
  `packages/backend/src/routes/inference/models-ui.ts`.
- `backend/data/provider-presets.json` — the provider preset catalog from Plexus
  `packages/backend/data/`.
- `backend/migrations/` — the PostgreSQL schema history ported from Plexus
  `packages/backend/drizzle/migrations_pg` (Trefoil-specific migrations are
  appended; see `backend/migrations/README.md`).
- Other backend modules (management API, OAuth login flows, MCP, quotas,
  cooldowns, usage, logging) and parts of `frontend/` (for example
  `frontend/src/contexts/AuthContext.tsx`, `frontend/src/lib/api.ts`,
  `frontend/src/lib/types.ts`)
  that port Plexus behaviour and are marked as such in their source comments.

```text
MIT License

Copyright (c) 2025 Matt Cowger

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## pi-ai (`@earendil-works/pi-ai`)

- Project: <https://github.com/earendil-works/pi> (`packages/ai`)
- Version: 1.2.0 (the version pinned by the Plexus reference revision)
- Author: Mario Zechner
- License: MIT (as declared in the package's `package.json`)

Plexus depends on pi-ai for its model catalog and OAuth login flows. Trefoil
embeds data generated from pi-ai and ports parts of its logic:

- `backend/src/assets/pi-catalog.json` — a snapshot of pi-ai's built-in model
  and provider catalog.
- `backend/src/engine/dispatch/pi_models.json` — pi-ai's built-in chat model
  metadata (selected fields).
- OAuth login flows in `backend/src/` (for example `kimi_oauth.rs`,
  `xai_oauth.rs`, `openai_chatgpt_oauth.rs`, `oauth_login.rs`) ported from
  pi-ai `auth/oauth/`.

```text
MIT License

Copyright (c) Mario Zechner

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Rust and npm dependencies

Rust crates (see `Cargo.lock`) and npm packages (see
`frontend/package-lock.json`) are fetched at build time and are distributed
under their own licenses; they are not vendored in this repository.
