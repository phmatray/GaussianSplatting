import Foundation
import Security
import SplatCaptureKit

/// State of the /ingest connection as the interface shows it.
enum LinkState: Equatable {
    case disconnected
    case connecting
    /// The server said `welcome`.
    case connected
    /// The French text says why; the link retries by itself unless stopped.
    case failed(String)
}

/// Everything that talks to the server: the /ingest WebSocket, the upload of
/// a recording to /api/captures and the reachability test.
///
/// The server certificate is self-signed. It is accepted if and only if the
/// SHA-256 of its DER encoding equals the `fp` of the pairing; any other
/// certificate is refused, a valid public one included.
///
/// Threading: the URLSession delivers every delegate call and completion
/// handler on the main queue, and the public methods must be called on the
/// main thread. Nothing here needs a lock.
final class ServerLink: NSObject, URLSessionWebSocketDelegate {
    var onState: ((LinkState) -> Void)?
    var onMessage: ((ServerMessage) -> Void)?
    /// Fraction sent, 0...1.
    var onUploadProgress: ((Double) -> Void)?
    /// nil on success, otherwise a French explanation.
    var onUploadFinished: ((String?) -> Void)?

    private(set) var state: LinkState = .disconnected
    private(set) var pairing: Pairing?
    /// True while the link keeps trying to (re)connect by itself.
    var isRetrying: Bool {
        return wantsConnection
    }

    private var session: URLSession?
    private var socket: URLSessionWebSocketTask?
    private var uploadTask: URLSessionTask?
    private var helloText: String?
    private var wantsConnection = false
    private var retryDelay: TimeInterval = 1
    private var retryWork: DispatchWorkItem?
    private var pingTimer: Timer?
    private var pinMismatch = false
    private var connectWatchdog: DispatchWorkItem?
    /// Seconds allowed between opening the socket and the server's `welcome`.
    private let connectTimeout: TimeInterval = 8

    // MARK: Pairing

    /// Sets (or clears) the server. Open connections are dropped and a new
    /// URLSession is made, so that nothing learned about the previous
    /// certificate is reused.
    func use(pairing: Pairing?) {
        if pairing == self.pairing && session != nil {
            return
        }
        disconnect()
        uploadTask?.cancel()
        uploadTask = nil
        session?.invalidateAndCancel()
        session = nil
        self.pairing = pairing
        guard pairing != nil else {
            return
        }
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 30
        configuration.waitsForConnectivity = false
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        session = URLSession(configuration: configuration, delegate: self, delegateQueue: OperationQueue.main)
    }

    // MARK: Certificate pinning

    func urlSession(
        _ session: URLSession,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        evaluate(challenge, completionHandler: completionHandler)
    }

    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        evaluate(challenge, completionHandler: completionHandler)
    }

    private func evaluate(
        _ challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust else {
            completionHandler(.performDefaultHandling, nil)
            return
        }
        guard let trust = challenge.protectionSpace.serverTrust,
              let expected = pairing?.fingerprint,
              let chain = SecTrustCopyCertificateChain(trust) as? [SecCertificate],
              let leaf = chain.first else {
            pinMismatch = true
            completionHandler(.cancelAuthenticationChallenge, nil)
            return
        }
        let der = SecCertificateCopyData(leaf) as Data
        if CertificateFingerprint.sha256Hex(der) == expected {
            // The system's own evaluation is not consulted: it cannot
            // succeed for a self-signed certificate, and the fingerprint
            // is the stronger statement anyway.
            completionHandler(.useCredential, URLCredential(trust: trust))
        } else {
            pinMismatch = true
            completionHandler(.cancelAuthenticationChallenge, nil)
        }
    }

    // MARK: /ingest

    /// Opens the stream and keeps it open (reconnecting with a growing
    /// delay) until `disconnect()`. `hello` is sent on every connection.
    func connect(hello: String) {
        helloText = hello
        wantsConnection = true
        retryDelay = 1
        open()
    }

    /// Stops reconnecting now and closes the stream half a second later, so
    /// that the server's last messages (acks, progress) can still arrive.
    func disconnectSoon() {
        wantsConnection = false
        retryWork?.cancel()
        retryWork = nil
        stopWatchdog()
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in
            // A new capture may have started meanwhile.
            guard let self = self, !self.wantsConnection else {
                return
            }
            self.disconnect()
        }
    }

    /// Closes the stream and stops reconnecting.
    func disconnect() {
        wantsConnection = false
        retryWork?.cancel()
        retryWork = nil
        stopWatchdog()
        stopPing()
        if let socket = socket {
            self.socket = nil
            socket.cancel(with: .normalClosure, reason: nil)
        }
        setState(.disconnected)
    }

    func send(text: String) {
        guard let socket = socket else {
            return
        }
        socket.send(.string(text)) { _ in
            // A failure ends the task; urlSession(_:task:didCompleteWithError:) reports it.
        }
    }

    func send(frame: Data) {
        guard let socket = socket else {
            return
        }
        socket.send(.data(frame)) { _ in
        }
    }

    private func open() {
        guard wantsConnection else {
            return
        }
        guard let pairing = pairing, let session = session, let url = pairing.ingestURL else {
            wantsConnection = false
            setState(.failed("Aucun serveur jumelé."))
            return
        }
        if let previous = socket {
            socket = nil
            previous.cancel(with: .goingAway, reason: nil)
        }
        pinMismatch = false
        let task = session.webSocketTask(with: url)
        socket = task
        setState(.connecting)
        task.resume()
        receive(on: task)
        startWatchdog(for: task)
    }

    /// The system waits a long time for an address that does not answer
    /// (up to `timeoutIntervalForRequest`), and the user is filming
    /// meanwhile: give up when no `welcome` came within `connectTimeout`.
    private func startWatchdog(for task: URLSessionWebSocketTask) {
        stopWatchdog()
        let work = DispatchWorkItem { [weak self] in
            guard let self = self, task === self.socket, self.state == .connecting else {
                return
            }
            task.cancel(with: .goingAway, reason: nil)
            self.socketEnded(reason: "Le serveur ne répond pas. L'iPhone et l'ordinateur sont-ils sur le même réseau Wi-Fi ?")
        }
        connectWatchdog = work
        DispatchQueue.main.asyncAfter(deadline: .now() + connectTimeout, execute: work)
    }

    private func stopWatchdog() {
        connectWatchdog?.cancel()
        connectWatchdog = nil
    }

    private func receive(on task: URLSessionWebSocketTask) {
        task.receive { [weak self] result in
            guard let self = self, task === self.socket else {
                return
            }
            switch result {
            case .failure:
                // Reported by urlSession(_:task:didCompleteWithError:).
                return
            case .success(let message):
                switch message {
                case .string(let text):
                    self.handle(text: text)
                case .data:
                    break
                @unknown default:
                    break
                }
                self.receive(on: task)
            }
        }
    }

    private func handle(text: String) {
        guard let message = ServerMessage.parse(text) else {
            return
        }
        switch message {
        case .welcome(let protocolVersion, _):
            if protocolVersion != captureProtocolVersion {
                fail(permanently: "Le serveur parle une autre version du protocole (\(protocolVersion)). Mettez à jour l'application ou le serveur.")
                return
            }
            retryDelay = 1
            stopWatchdog()
            onMessage?(message)
            setState(.connected)
        case .error(let code, let detail):
            let explanation: String
            if code == "bad_token" {
                explanation = "Le serveur refuse le jeton de jumelage (il a sans doute redémarré). Scannez de nouveau le QR code."
            } else {
                explanation = "Le serveur a refusé la connexion : \(detail.isEmpty ? code : detail)"
            }
            onMessage?(message)
            fail(permanently: explanation)
        default:
            onMessage?(message)
        }
    }

    /// An error that retrying cannot fix.
    private func fail(permanently text: String) {
        wantsConnection = false
        retryWork?.cancel()
        retryWork = nil
        stopWatchdog()
        stopPing()
        if let socket = socket {
            self.socket = nil
            socket.cancel(with: .normalClosure, reason: nil)
        }
        setState(.failed(text))
    }

    func urlSession(
        _ session: URLSession,
        webSocketTask: URLSessionWebSocketTask,
        didOpenWithProtocol negotiatedProtocol: String?
    ) {
        guard webSocketTask === socket, let hello = helloText else {
            return
        }
        send(text: hello)
        startPing()
    }

    func urlSession(
        _ session: URLSession,
        webSocketTask: URLSessionWebSocketTask,
        didCloseWith closeCode: URLSessionWebSocketTask.CloseCode,
        reason: Data?
    ) {
        guard webSocketTask === socket else {
            return
        }
        socketEnded(reason: "Le serveur a fermé la connexion.")
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        if task === uploadTask {
            uploadTask = nil
            if let error = error {
                onUploadFinished?(describe(error))
            } else if let response = task.response as? HTTPURLResponse {
                if (200..<300).contains(response.statusCode) {
                    onUploadFinished?(nil)
                } else if response.statusCode == 401 || response.statusCode == 403 {
                    onUploadFinished?("Le serveur refuse le jeton de jumelage. Scannez de nouveau le QR code.")
                } else {
                    onUploadFinished?("Le serveur a refusé le fichier (code HTTP \(response.statusCode)).")
                }
            } else {
                onUploadFinished?("Le serveur n'a pas répondu.")
            }
            return
        }
        guard task === socket else {
            return
        }
        if let error = error {
            socketEnded(reason: describe(error))
        } else {
            socketEnded(reason: "Connexion au serveur terminée.")
        }
    }

    private func socketEnded(reason: String) {
        socket = nil
        stopWatchdog()
        stopPing()
        guard wantsConnection else {
            setState(.disconnected)
            return
        }
        if pinMismatch {
            fail(permanently: reason)
            return
        }
        // Schedule the retry first: the listener may call disconnect() from
        // inside onState, which cancels it.
        let delay = retryDelay
        retryDelay = min(retryDelay * 2, 8)
        let work = DispatchWorkItem { [weak self] in
            self?.open()
        }
        retryWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: work)
        setState(.failed(reason))
    }

    private func setState(_ newState: LinkState) {
        if newState == state {
            return
        }
        state = newState
        onState?(newState)
    }

    // A WebSocket without traffic may be dropped by the system or by a
    // router; a ping every few seconds also notices a dead connection.
    private func startPing() {
        stopPing()
        pingTimer = Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { [weak self] _ in
            guard let task = self?.socket else {
                return
            }
            task.sendPing { error in
                guard error != nil else {
                    return
                }
                // No pong: the connection is dead even if the system has
                // not noticed yet. Reconnect instead of skipping frames.
                DispatchQueue.main.async {
                    guard let self = self, task === self.socket else {
                        return
                    }
                    task.cancel(with: .goingAway, reason: nil)
                    self.socketEnded(reason: "Connexion au serveur perdue.")
                }
            }
        }
    }

    private func stopPing() {
        pingTimer?.invalidate()
        pingTimer = nil
    }

    // MARK: Reachability

    /// Asks the server for /api/info. Any HTTP answer through the pinned
    /// TLS connection counts as "reachable". The second value explains a
    /// failure in French.
    func probe(completion: @escaping (Bool, String?) -> Void) {
        guard let pairing = pairing, let session = session, let url = pairing.infoURL else {
            completion(false, "Aucun serveur jumelé.")
            return
        }
        pinMismatch = false
        var request = URLRequest(url: url)
        request.timeoutInterval = 6
        let task = session.dataTask(with: request) { [weak self] _, response, error in
            if let error = error {
                completion(false, self?.describe(error))
            } else {
                completion(response is HTTPURLResponse, nil)
            }
        }
        task.resume()
    }

    // MARK: Upload of a recording

    var isUploading: Bool {
        return uploadTask != nil
    }

    /// POSTs a .gscap to /api/captures with the pairing token.
    func upload(file: URL) {
        guard uploadTask == nil else {
            return
        }
        guard let pairing = pairing, let session = session, let url = pairing.capturesURL else {
            onUploadFinished?("Aucun serveur jumelé.")
            return
        }
        pinMismatch = false
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.timeoutInterval = 600
        request.setValue("Bearer \(pairing.token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
        let task = session.uploadTask(with: request, fromFile: file)
        uploadTask = task
        onUploadProgress?(0)
        task.resume()
    }

    func cancelUpload() {
        uploadTask?.cancel()
    }

    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        didSendBodyData bytesSent: Int64,
        totalBytesSent: Int64,
        totalBytesExpectedToSend: Int64
    ) {
        guard task === uploadTask, totalBytesExpectedToSend > 0 else {
            return
        }
        onUploadProgress?(Double(totalBytesSent) / Double(totalBytesExpectedToSend))
    }

    // MARK: Errors in French

    private func describe(_ error: Error) -> String {
        let nsError = error as NSError
        if pinMismatch {
            return "Le certificat du serveur ne correspond pas au jumelage. Scannez de nouveau le QR code affiché par le serveur."
        }
        // iOS gives no API to read the local network permission. A refusal
        // shows up as "offline" with this note in the underlying error.
        let details = String(describing: nsError.userInfo)
        if details.contains("Local network prohibited") {
            return "L'accès au réseau local est refusé. Autorisez-le dans Réglages > Confidentialité et sécurité > Réseau local > SplatCapture."
        }
        guard nsError.domain == NSURLErrorDomain else {
            return nsError.localizedDescription
        }
        switch nsError.code {
        case NSURLErrorNotConnectedToInternet:
            return "Pas de réseau, ou accès au réseau local refusé. Vérifiez le Wi-Fi, puis Réglages > Confidentialité et sécurité > Réseau local > SplatCapture."
        case NSURLErrorCannotConnectToHost:
            return "Serveur injoignable : connexion refusée. Le serveur est-il lancé, avec l'accès au réseau local activé ?"
        case NSURLErrorTimedOut:
            return "Le serveur ne répond pas. L'iPhone et l'ordinateur sont-ils sur le même réseau Wi-Fi ?"
        case NSURLErrorCannotFindHost, NSURLErrorDNSLookupFailed:
            return "Adresse du serveur introuvable."
        case NSURLErrorNetworkConnectionLost:
            return "Connexion au serveur perdue."
        case NSURLErrorSecureConnectionFailed, NSURLErrorServerCertificateUntrusted,
             NSURLErrorServerCertificateHasBadDate, NSURLErrorServerCertificateHasUnknownRoot,
             NSURLErrorServerCertificateNotYetValid:
            return "Connexion sécurisée impossible avec le serveur."
        case NSURLErrorCancelled:
            return "Connexion annulée."
        default:
            return nsError.localizedDescription
        }
    }
}
