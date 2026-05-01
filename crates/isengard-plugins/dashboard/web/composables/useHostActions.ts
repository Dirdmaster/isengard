export function useHostActions() {
  const api = useApi()

  async function setFleet(hostId: string, fleet: string) {
    return await api.patch(`/hosts/${hostId}`, { fleet })
  }

  async function forceUpdate(hostId: string) {
    return await api.post(`/hosts/${hostId}/actions/force-update`, {})
  }

  async function decommission(hostId: string) {
    return await api.delete(`/hosts/${hostId}`)
  }

  return { setFleet, forceUpdate, decommission }
}
