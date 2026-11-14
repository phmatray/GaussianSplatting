# Vérifications de bout en bout du serveur

- `tauri-smoke.sh` : compile la coquille Tauri avec la page de test `standin/`
  à la place de l'interface, la lance sous Xvfb et vérifie la commande
  `server_info`, les requêtes HTTP depuis la fenêtre et l'arrivée des trames
  WebSocket. Voir `src-tauri/README.md`.
- `browser.cjs` : lance le binaire `gs-server` et ouvre `standin/` dans
  Chromium (Playwright), en HTTP local puis en HTTPS avec le certificat
  auto-signé, comme le ferait le navigateur d'un casque.
- `real-frontend.cjs` : la même chose avec l'interface réelle
  (`crates/gs-app/dist`) : source « Serveur natif », flux reçu par le
  WebSocket et splats dessinés, en HTTP local puis en HTTPS sur l'adresse du
  réseau local. Ignoré si l'interface n'est pas construite.
- `tauri-real.sh` : la coquille Tauri avec l'interface réelle, sous Xvfb.
  Vérifie dans le journal et sur une capture d'écran que la fenêtre affiche
  l'interface, qu'elle utilise le serveur embarqué et que les splats sont
  dessinés. Ignoré (code 77) si l'interface n'est pas construite.

- `abuse.py` : malmène le binaire `gs-server` depuis l'extérieur et mesure
  ce que les tests d'intégration ne voient pas de l'intérieur : fils
  d'exécution, descripteurs de fichiers et mémoire avant et après 400
  connexions et déconnexions, 300 connexions muettes ou au compte-gouttes,
  trames WebSocket malformées, en-têtes `Origin` et `Host` maquillés, 180
  bascules de l'accès réseau pendant que des clients se reconnectent en TLS,
  arrêt en pleine charge. Il termine par un scan complet de la maison : un
  client arrivé après la fin doit détenir, octet pour octet, la même couche
  que celui qui a tout suivi. Python seul, sans dépendance.

```bash
cargo build --release -p gs-server
node e2e/server/browser.cjs          # ports 7860 et 7861
python3 e2e/server/abuse.py          # ports 7880 à 7883, environ 1 min 30
e2e/server/tauri-smoke.sh

(cd crates/gs-app && trunk build --release)
node e2e/server/real-frontend.cjs    # ports 7870 et 7871
e2e/server/tauri-real.sh
```

Les tests d'intégration du serveur lui-même (sockets réels, sans navigateur)
sont dans `crates/gs-server/tests` : `cargo test -p gs-server`. Les mesures de
charge se lancent avec
`cargo run --release -p gs-server --example measure`.
