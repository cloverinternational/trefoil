# Frontend test parity

The eight helper areas are covered by Vitest: `format`, `currency`, `api`, `modelOptions`, `modelList`, `normalize`, `systemLogs`, and `detailed-usage/helpers` (`src/lib/*.test.ts`, `src/pages/detailed-usage/__tests__/helpers.test.ts`). Currency cache/error cases, model search/empty targets, and detailed usage controls/metrics/ranges are covered.

Run `npm run build && npm run test:e2e` for deterministic, service-free Playwright tests against Vite preview. `e2e/control-plane.spec.ts` covers auth success/failure, navigation/search, usage records, CRUD form path, API errors, and ordered SSE. Backend persistence and multi-browser coverage remain out of scope.
