class AuthorizedClient {
    constructor(wasmInstance, apiBase) {
        this.wasmInstance = wasmInstance;
        this.apiBase = apiBase;
    }

    async _getAuthHeader(url, method, body) {
        return this.wasmInstance.getAuthHeader(url, method, body);
    }

    async _request(url, method, body, options = {}) {
        const authHeader = await this._getAuthHeader(url, method, body);
        const response = await fetch(url, {
            ...options,
            method,
            headers: {
                'Content-Type': 'application/json',
                ...options.headers,
                'Authorization': authHeader,
            },
            body: body ? JSON.stringify(body) : undefined,
        });

        if (!response.ok) {
            throw new Error(`HTTP error! status: ${response.status}`);
        }
        return response;
    }

    get(url, options = {}) {
        return this._request(url, 'GET', null, options);
    }

    post(url, body = null, options = {}) {
        return this._request(url, 'POST', body, options);
    }

    put(url, body = null, options = {}) {
        return this._request(url, 'PUT', body, options);
    }

    delete(url, body = null, options = {}) {
        return this._request(url, 'DELETE', body, options);
    }
}

window.AuthorizedClient = AuthorizedClient;
