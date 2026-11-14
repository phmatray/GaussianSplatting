# Vérifications de bout en bout du serveur

- `tauri-smoke.sh` : compile la coquille Tauri avec la page de test `standin/`
  à la place de l'interface, la lance sous Xvfb et vérifie la commande
  `server_info`, les requêtes HTTP depuis la fenêtre, l'arrivée des trames
  WebSocket, puis l'enregistrement : `POST /api/record`, lecture d'un
  enregistrement par plages d'octets depuis l'origine de la fenêtre, QR code
  de jumelage, et les deux exports de la commande `export_recording` (le
  fichier `.gsrec` et l'archive de partage, relus avec `tools/gsrec.py` et
  `zipfile`). Voir `src-tauri/README.md`.
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

- `capture.py` : la capture réelle de bout en bout avec les outils de
  référence. Lance le binaire `gs-server` et joue le rôle du téléphone avec
  `tools/fake_phone.py` : TLS avec le certificat épinglé sur l'empreinte du
  jumelage, protocole `/ingest` et son contrôle de flux, envois vers
  `/api/captures`. Un spectateur WebSocket suit tout du long. Vérifie le
  jumelage, une marche synthétique dans les quatre modes, les captures du
  téléphone simulé de gs-core (si `target/release/examples/gscap_write`
  existe), le mauvais jeton, la mauvaise empreinte, la violation du contrôle
  de flux, le second téléphone, la remise à zéro, la reconnexion en pleine
  capture, les imports, une capture 1920x1440 à dix images par seconde
  pendant que l'API est chronométrée, puis relit ce que le serveur a laissé
  sur le disque avec `tools/gscap.py` et `tools/gsrec.py`. Python seul.
- `refine.py` : l'affinage hors ligne et les exports par le serveur. Envoie
  une petite capture synthétique (`tools/gscap.py synth`), la retrouve dans
  les captures conservées, l'affine avec le préréglage rapide réduit à
  quelques dizaines d'itérations (`--refine-iterations`), annule un calcul,
  exporte en COLMAP et en nerfstudio, puis liste, télécharge par plages
  d'octets, emballe dans une archive de partage et supprime le résultat. Le
  serveur doit répondre à `/api/info` pendant tout le calcul et s'arrêter
  proprement avec un calcul en cours. Python seul.
- `hostile.py` : les routes de capture, d'enregistrement et de partage face
  à un client malveillant. Un inconnu du réseau local (jeton absent, faux,
  tronqué, dans l'URL ; message de 9 Mio ; connexions muettes sur
  `/ingest`), un téléphone jumelé qui enfreint le protocole (quarante images
  d'un coup, bruit, tailles falsifiées, déluge de messages) ou qui disparaît
  au milieu d'une image, une page web d'une autre origine sur la machine
  elle-même (`Origin`, `Host` : CSRF et DNS rebinding), des envois géants,
  interrompus ou qui n'avancent plus, puis toutes les formes de `Range` sur
  un enregistrement terminé et sur un fichier qui grossit, et une trentaine
  de noms de fichiers piégés sur trois routes. Vérifie à la fin que le jeton
  de jumelage n'apparaît nulle part dans la sortie du serveur. Python seul ;
  `--quick` saute les attentes des délais du serveur.
- `share.cjs` : le partage de bout en bout. Enregistre quelques secondes de
  la scène (`POST /api/record`) et une capture diffusée par
  `tools/fake_phone.py`, télécharge l'archive de partage de chacune
  (`GET /api/share/<nom>.zip`), la décompresse, la sert avec
  `python3 -m http.server` et vérifie dans Chromium que l'interface réelle
  démarre toute seule sur l'enregistrement et le rejoue ; puis ouvre le lien
  du réseau local (`https://<adresse>/?file=recordings/<nom>.gsrec&embed=1`).
  Captures d'écran dans `$OUT`. Ignoré si l'interface n'est pas construite.

```bash
cargo build --release -p gs-server
node e2e/server/browser.cjs          # ports 7860 et 7861
python3 e2e/server/abuse.py          # ports 7880 à 7883, environ 1 min 30
e2e/server/tauri-smoke.sh

cargo build --release -p gs-core --features par --example gscap_write   # facultatif : téléphone simulé
python3 e2e/server/capture.py        # ports 7890 et 7891, environ 1 min
python3 e2e/server/hostile.py        # ports 7940 et 7941, environ 3 min (--quick : 40 s)
python3 e2e/server/refine.py         # ports 7960 et 7961, environ 20 s

(cd crates/gs-app && trunk build --release)
node e2e/server/real-frontend.cjs    # ports 7870 et 7871
node e2e/server/share.cjs            # ports 7895 à 7898, environ 2 min
e2e/server/tauri-real.sh
```

Les tests d'intégration du serveur lui-même (sockets réels, sans navigateur)
sont dans `crates/gs-server/tests` : `cargo test -p gs-server`. Ceux de la
capture (`capture.rs`) utilisent le téléphone simulé des tests de gs-core ;
ceux des enregistrements (`recordings.rs`) vérifient les plages d'octets,
les noms de fichiers hostiles et l'archive de partage ; `refine.rs` vérifie
l'affinage, son annulation, ses refus, les exports et l'état du disque ;
`hardening.rs`
vérifie la limite de connexions de l'accès réseau et le contrôle de
l'en-tête `Host` sur l'accès local. Les mesures de
charge se lancent avec
`cargo run --release -p gs-server --example measure`.
