export type CatalogKind = 'provider' | 'model' | 'alias' | 'key';
export type CatalogEntry = { group: string | null; tags: string[] };
export type CatalogOrganization = { groups: string[]; items: Record<CatalogKind, Record<string, CatalogEntry>> };

const base = '/v0/management/catalog-organization';
async function request(path: string, init: RequestInit = {}) {
  const response = await fetch(`${base}${path}`, { ...init, headers: { 'x-admin-key': localStorage.getItem('trefoil_admin_key') ?? '', 'content-type': 'application/json', ...init.headers } });
  if (!response.ok) {
    const body = await response.json().catch(() => null) as { error?: string | { message?: string } } | null;
    const message = typeof body?.error === 'string' ? body.error : body?.error?.message;
    throw new Error(message || `Organization request failed (${response.status})`);
  }
  return response;
}

export async function getCatalogOrganization(): Promise<CatalogOrganization> {
  return (await request('')).json();
}
export async function createCatalogGroup(name: string): Promise<void> {
  await request(`/groups/${encodeURIComponent(name)}`, { method: 'PUT' });
}
export async function deleteCatalogGroup(name: string): Promise<void> {
  await request(`/groups/${encodeURIComponent(name)}`, { method: 'DELETE' });
}
export async function saveCatalogEntry(kind: CatalogKind, id: string, entry: CatalogEntry): Promise<void> {
  await request(`/${kind}/${encodeURIComponent(id)}`, { method: 'PUT', body: JSON.stringify(entry) });
}
