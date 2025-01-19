class AuthorizedClient {
  constructor(wasmInstance, apiBase) {
    this.wasmInstance = wasmInstance;
    this.apiBase = apiBase;
  }

  async _getAuthHeader(url, method, body) {
    return this.wasmInstance.getAuthHeader(url, method, body);
  }

  async get(url, options = {}) {
    const authHeader = await this._getAuthHeader(url, "GET", options.body);
    console.log(authHeader);
    return fetch(url, {
      ...options,
      method: "GET",
      headers: {
        "Content-Type": "application/json",
        ...options.headers,
        Authorization: authHeader,
      },
    });
  }

  async post(url, body = null, options = {}) {
    const authHeader = await this._getAuthHeader(url, "POST", body);

    return fetch(url, {
      ...options,
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        ...options.headers,
        Authorization: authHeader,
      },
      body: JSON.stringify(body),
    });
  }

  async put(url, body = null, options = {}) {
    const authHeader = await this._getAuthHeader(url, "PUT", body);

    return fetch(url, {
      ...options,
      method: "PUT",
      headers: {
        "Content-Type": "application/json",
        ...options.headers,
        Authorization: authHeader,
      },
      body: JSON.stringify(body),
    });
  }

  async delete(url, body = null, options = {}) {
    const authHeader = await this._getAuthHeader(url, "DELETE", body);

    return fetch(url, {
      ...options,
      method: "DELETE",
      headers: {
        "Content-Type": "application/json",
        ...options.headers,
        Authorization: authHeader,
      },
      body: body ? JSON.stringify(body) : undefined,
    });
  }
}

export { AuthorizedClient };
