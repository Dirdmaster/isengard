const API_BASE = '/api/v1'

export function useApi() {
  return {
    get<T>(path: string, query?: Record<string, any>) {
      return $fetch<T>(`${API_BASE}${path}`, { query })
    },
    post<T>(path: string, body?: any) {
      return $fetch<T>(`${API_BASE}${path}`, { method: 'POST', body })
    },
    put<T>(path: string, body?: any) {
      return $fetch<T>(`${API_BASE}${path}`, { method: 'PUT', body })
    },
    patch<T>(path: string, body?: any) {
      return $fetch<T>(`${API_BASE}${path}`, { method: 'PATCH', body })
    },
    delete<T>(path: string) {
      return $fetch<T>(`${API_BASE}${path}`, { method: 'DELETE' })
    },
  }
}
