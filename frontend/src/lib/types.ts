// Ported from Plexus packages/frontend/src/types/settings.ts (Principal type
// only) and packages/backend/src/routes/management/_principal.ts. Mirrors
// the exact JSON shape returned by GET /v0/management/auth/verify.
export type Principal =
  | { role: 'admin' }
  | {
      role: 'limited';
      keyName: string;
      allowedProviders: string[];
      allowedModels: string[];
      excludedProviders: string[];
      excludedModels: string[];
      quotaName: string | null;
      comment: string | null;
    };
