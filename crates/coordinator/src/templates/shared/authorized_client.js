class AuthorizedClient {
    constructor(wasmInstance, apiBase) {
        this.wasmInstance = wasmInstance;
        this.apiBase = apiBase;
    }

    async _request(url, method, body, options = {}) {
        // Serialize once: the NIP-98 payload hash must cover the exact bytes sent.
        const payload = body ? JSON.stringify(body) : null;
        const authHeader = await this.wasmInstance.getAuthHeader(url, method, payload);
        const response = await fetch(url, {
            ...options,
            method,
            headers: {
                'Content-Type': 'application/json',
                ...options.headers,
                'Authorization': authHeader,
            },
            body: payload ?? undefined,
        });

        if (!response.ok) {
            const error = new Error(`HTTP error! status: ${response.status}`);
            error.response = response;
            throw error;
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
