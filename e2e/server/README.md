# Vérifications de bout en bout du serveur

- `tauri-smoke.sh` : compile la coquille Tauri avec la page de test `standin/`
  à la place de l'interface, la lance sous Xvfb et vérifie la commande
  `server_info`, les requêtes HTTP depuis la fenêtre et l'arrivée des trames
  WebSocket. Voir `src-tauri/README.md`.
- `browser.cjs` : lance le binaire `gs-server` et ouvre `standin/` dans
  Chromium (Playwright), en HTTP local puis en HTTPS avec le certificat
  auto-signé, comme le ferait le navigateur d'un casque.

```bash
cargo build --release -p gs-server
node e2e/server/browser.cjs          # ports 7860 et 7861
e2e/server/tauri-smoke.sh
```

Les tests d'intégration du serveur lui-même (sockets réels, sans navigateur)
sont dans `crates/gs-server/tests` : `cargo test -p gs-server`. Les mesures de
charge se lancent avec
`cargo run --release -p gs-server --example measure`.
