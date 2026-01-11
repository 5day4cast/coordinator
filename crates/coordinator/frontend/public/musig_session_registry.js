import { MusigSessionManager } from "./musig_session_manager.js";

class MusigSessionRegistry {
  constructor() {
    console.log("Initializing MusigSessionRegistry");

    this.sessions = new Map();
    this.observers = new Set();
  }

  updateSessionInfo(competitionId, entryId, entryIndex, fundingOutpoint) {
    const session = this.getSession(competitionId, entryId);
    if (session) {
      if (entryIndex !== undefined) {
        session.entryIndex = entryIndex;
      }
      if (fundingOutpoint !== undefined) {
        session.fundingOutpoint = fundingOutpoint;
      }
      this.notifyObservers();
    }
  }

  addObserver(observer) {
    this.observers.add(observer);
    if (observer.setRegistry) {
      observer.setRegistry(this);
    }
  }

  notifyObservers() {
    this.observers.forEach((observer) => {
      if (observer.updateUI) {
        observer.updateUI();
      }
    });
  }

  removeObserver(observer) {
    this.observers.delete(observer);
  }

  async createSession(wallet, competitionId, entryId, client, entryIndex) {
    console.log("Creating session for:", competitionId, entryId);
    const sessionKey = this.getSessionKey(competitionId, entryId);

    if (this.sessions.has(sessionKey)) {
      console.log("Session already exists:", sessionKey);
      return this.sessions.get(sessionKey);
    }

    const manager = new MusigSessionManager(
      wallet,
      competitionId,
      entryId,
      client,
      entryIndex,
      (session) => this.handleSessionStateChange(session),
    );
    console.log("Created new session manager:", manager);

    this.sessions.set(sessionKey, manager);
    this.notifyObservers();

    manager.start().catch((error) => {
      console.error(`Error in signing session ${sessionKey}:`, error);
      this.removeSession(competitionId, entryId);
    });

    return manager;
  }

  handleSessionStateChange(session) {
    console.log(
      `Session ${session.competitionId}_${session.entryId} state changed to: ${session.state}`,
    );
    this.notifyObservers();
  }

  getSession(competitionId, entryId) {
    return this.sessions.get(this.getSessionKey(competitionId, entryId));
  }

  removeSession(competitionId, entryId) {
    const sessionKey = this.getSessionKey(competitionId, entryId);
    this.sessions.delete(sessionKey);
  }

  getAllSessions() {
    return Array.from(this.sessions.values());
  }

  getActiveSessions() {
    console.log("Getting active sessions");
    console.log("Current sessions:", this.sessions);

    const activeSessions = Array.from(this.sessions.values()).filter(
      (session) => {
        if (!session) {
          console.warn("Found null or undefined session");
          return false;
        }

        if (typeof session.getState !== "function") {
          console.warn("Session missing getState method:", session);
          return false;
        }

        const state = session.getState();
        console.log("Session state for", session.entryId, ":", state);
        return state !== "COMPLETED" && state !== "ERROR";
      },
    );

    console.log("Active sessions:", activeSessions);
    return activeSessions;
  }

  getSessionKey(competitionId, entryId) {
    return `${competitionId}_${entryId}`;
  }

  clearAllSessions() {
    console.log("Clearing all signing sessions");
    this.sessions.clear();
    this.notifyObservers();
  }
}

export { MusigSessionRegistry };
