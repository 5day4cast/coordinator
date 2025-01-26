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
    try {
      const response = await fetch(url, {
        ...options,
        method: "GET",
        headers: {
          "Content-Type": "application/json",
          ...options.headers,
          Authorization: authHeader,
        },
      }).then((res) => {
        if (!res.ok) {
          throw new Error(`HTTP error! status: ${res.status}`);
        }
        return res;
      });
      return response;
    } catch (error) {
      throw error;
    }
  }

  async post(url, body = null, options = {}) {
    const authHeader = await this._getAuthHeader(url, "POST", body);
    try {
      const response = await fetch(url, {
        ...options,
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          ...options.headers,
          Authorization: authHeader,
        },
        body: JSON.stringify(body),
      }).then((res) => {
        if (!res.ok) {
          throw new Error(`HTTP error! status: ${res.status}`);
        }
        return res;
      });
      return response;
    } catch (error) {
      throw error;
    }
  }

  async put(url, body = null, options = {}) {
    const authHeader = await this._getAuthHeader(url, "PUT", body);
    try {
      const response = await fetch(url, {
        ...options,
        method: "PUT",
        headers: {
          "Content-Type": "application/json",
          ...options.headers,
          Authorization: authHeader,
        },
        body: JSON.stringify(body),
      }).then((res) => {
        if (!res.ok) {
          throw new Error(`HTTP error! status: ${res.status}`);
        }
        return res;
      });
      return response;
    } catch (error) {
      throw error;
    }
  }

  async delete(url, body = null, options = {}) {
    const authHeader = await this._getAuthHeader(url, "DELETE", body);
    try {
      const response = await fetch(url, {
        ...options,
        method: "DELETE",
        headers: {
          "Content-Type": "application/json",
          ...options.headers,
          Authorization: authHeader,
        },
        body: body ? JSON.stringify(body) : undefined,
      }).then((res) => {
        if (!res.ok) {
          throw new Error(`HTTP error! status: ${res.status}`);
        }
        return res;
      });
      return response;
    } catch (error) {
      throw error;
    }
  }
}

export { AuthorizedClient };
