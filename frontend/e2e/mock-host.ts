// Address the backend container uses to reach mock upstreams started by these
// specs on the test host (for a Linux Compose network, its gateway IP).
export const mockHost = process.env.TREFOIL_E2E_MOCK_HOST ?? 'host.docker.internal';
