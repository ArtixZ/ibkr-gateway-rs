package dev.ibkr.gateway;

import java.awt.Component;
import java.awt.Container;
import java.awt.Dialog;
import java.awt.Window;
import java.awt.event.WindowEvent;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.EOFException;
import java.io.IOException;
import java.lang.instrument.Instrumentation;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.LinkOption;
import java.nio.file.attribute.PosixFilePermissions;
import java.time.LocalTime;
import java.time.ZonedDateTime;
import java.time.format.DateTimeFormatter;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import javax.swing.AbstractButton;
import javax.swing.JCheckBox;
import javax.swing.JDialog;
import javax.swing.JEditorPane;
import javax.swing.JFrame;
import javax.swing.JLabel;
import javax.swing.JMenu;
import javax.swing.JMenuItem;
import javax.swing.JPasswordField;
import javax.swing.JRadioButton;
import javax.swing.JTextField;
import javax.swing.JTextArea;
import javax.swing.JToggleButton;
import javax.swing.JTree;
import javax.swing.SwingUtilities;
import javax.swing.Timer;
import javax.swing.tree.TreeModel;
import javax.swing.tree.TreePath;

public final class GatewayBridge {
    private static final int LIMIT = 65536;
    private static final ArrayBlockingQueue<String[]> EVENTS = new ArrayBlockingQueue<>(64);
    private static volatile String state = "starting";
    private static volatile String reason = "waiting_for_supervisor";
    private static volatile boolean connected;
    private static Path runtime;
    private static String generation;
    private static Ui ui;
    private static volatile long scheduledRestartEpoch;
    private static final CountDownLatch AUTHORIZED = new CountDownLatch(1);
    private static long bootTimestamp;
    private static volatile boolean shutdownRequested;
    private static final String NATIVE_SERVICE = "com.ib.tws.twslaunch.install4j.Install4jAutoUpdateService";

    private GatewayBridge() {}

    public static void main(String[] args) throws Exception {
        Class<?> gateway = Class.forName("ibgateway.GWClient", false,
                GatewayBridge.class.getClassLoader());
        gateway.getMethod("main", String[].class);
        if (args.length == 1 && args[0].equals("--inspect")) {
            Class.forName(NATIVE_SERVICE, false, GatewayBridge.class.getClassLoader())
                    .getMethod("restartJvm", String.class);
            System.out.println("bridge_protocol=3");
            System.out.println("gateway_entry=ibgateway.GWClient");
            System.out.println("restart_service=" + NATIVE_SERVICE);
            System.out.println("java_major=" + Runtime.version().feature());
            return;
        }
        if (args.length != 1) {
            throw new IllegalArgumentException("Expected one private settings directory");
        }
        bootstrap(Path.of(args[0]));
        gateway.getMethod("main", String[].class).invoke(null, (Object) new String[] {args[0]});
    }

    public static void premain(String arguments, Instrumentation instrumentation) throws Exception {
        bootstrap(Path.of(System.getProperty("jtsConfigDir")));
    }

    private static void bootstrap(Path settings) throws Exception {
        runtime = Path.of(System.getProperty("gatewayctl.runtime")).toRealPath();
        generation = System.getProperty("gatewayctl.generation", "");
        if (!generation.matches("[0-9a-f]{32}")
                || !settings.toRealPath().equals(runtime.resolve("settings").toRealPath())) {
            throw new IllegalArgumentException("Invalid instance bootstrap");
        }
        if (!NATIVE_SERVICE.equals(
                System.getProperty("twslaunch.autoupdate.serviceImpl"))) {
            throw new IllegalStateException("The vendor's native restart handler must be retained");
        }
        bootTimestamp = System.currentTimeMillis();
        Runtime.getRuntime().addShutdownHook(new Thread(GatewayBridge::recordNativeShutdown, "gatewayctl-native-shutdown"));
        Thread.setDefaultUncaughtExceptionHandler((thread, error) -> {
            emit("needs_attention", "uncaught_" + error.getClass().getSimpleName());
            System.err.println("Gateway bridge observed an uncaught exception in " + thread.getName());
        });
        SwingUtilities.invokeAndWait(() -> {
            ui = new Ui();
            new Timer(1000, event -> {
                try {
                    if (connected) {
                        ui.tick();
                    }
                    EVENTS.offer(new String[] {"PULSE"});
                } catch (UiFailure error) {
                    ui.blocked = true;
                    emit("needs_attention", error.getMessage());
                } catch (RuntimeException error) {
                    ui.blocked = true;
                    emit("needs_attention", "unsupported_ui_" + error.getClass().getSimpleName());
                }
            }).start();
        });
        Thread transport = new Thread(GatewayBridge::transport, "gatewayctl-transport");
        transport.setDaemon(true);
        transport.start();
        if (!AUTHORIZED.await(30, TimeUnit.SECONDS)) {
            emit("needs_attention", "supervisor_did_not_authorize_startup");
            System.exit(78);
            return;
        }
    }

    static synchronized void emit(String newState, String newReason) {
        if (state.equals(newState) && reason.equals(newReason)) {
            return;
        }
        state = newState;
        reason = newReason;
        if (!EVENTS.offer(new String[] {"STATE", state, reason})) {
            System.err.println("Gateway bridge event queue full; automation paused");
            connected = false;
        }
    }

    private static void transport() {
        while (!Thread.currentThread().isInterrupted()) {
            try (SocketChannel channel = SocketChannel.open(StandardProtocolFamily.UNIX)) {
                channel.connect(UnixDomainSocketAddress.of(runtime.resolve("bridge.sock")));
                write(channel, new String[] {"HELLO", "3", Long.toString(ProcessHandle.current().pid()), generation});
                String[] config = read(channel);
                if (config.length != 9 || !config[0].equals("CONFIG")) {
                    throw new IOException("Unexpected supervisor handshake");
                }
                SwingUtilities.invokeAndWait(() -> ui.configure(config));
                connected = true;
                AUTHORIZED.countDown();
                write(channel, reconnectSnapshot());
                Thread reader = new Thread(() -> receive(channel), "gatewayctl-commands");
                reader.setDaemon(true);
                reader.start();
                while (channel.isOpen()) {
                    String[] event = EVENTS.poll(1, TimeUnit.SECONDS);
                    if (event != null) {
                        write(channel, event);
                    }
                }
                reader.join(1000);
            } catch (IOException error) {
                connected = false;
                System.err.println("Gateway bridge connection unavailable; retrying private supervisor connection");
            } catch (InterruptedException error) {
                Thread.currentThread().interrupt();
            } catch (java.lang.reflect.InvocationTargetException error) {
                connected = false;
                emit("needs_attention", "invalid_supervisor_configuration");
            }
            try {
                Thread.sleep(2000);
            } catch (InterruptedException error) {
                Thread.currentThread().interrupt();
            }
        }
    }

    private static void receive(SocketChannel channel) {
        try {
            while (channel.isOpen()) {
                String[] command = read(channel);
                SwingUtilities.invokeAndWait(() -> ui.command(command));
                Arrays.fill(command, "");
            }
        } catch (IOException error) {
            connected = false;
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
        } catch (java.lang.reflect.InvocationTargetException error) {
            emit("needs_attention", "invalid_supervisor_command");
        } finally {
            try {
                channel.close();
            } catch (IOException error) {
                System.err.println("Gateway bridge socket close failed");
            }
        }
    }

    static synchronized String[] reconnectSnapshot() {
        EVENTS.clear();
        if (scheduledRestartEpoch != 0) {
            EVENTS.offer(new String[] {"RESTART_SCHEDULED", Long.toString(scheduledRestartEpoch)});
        }
        return new String[] {"STATE", state, reason};
    }

    static byte[] encode(String[] fields) throws IOException {
        if (fields.length == 0 || fields.length > 16) {
            throw new IOException("Invalid IPC field count");
        }
        ByteArrayOutputStream buffer = new ByteArrayOutputStream();
        try (DataOutputStream output = new DataOutputStream(buffer)) {
            output.writeShort(fields.length);
            for (String field : fields) {
                byte[] data = field.getBytes(StandardCharsets.UTF_8);
                output.writeInt(data.length);
                output.write(data);
            }
        }
        if (buffer.size() > LIMIT) {
            throw new IOException("IPC frame too large");
        }
        return buffer.toByteArray();
    }

    static String[] decode(byte[] body) throws IOException {
        try (DataInputStream input = new DataInputStream(new ByteArrayInputStream(body))) {
            int count = input.readUnsignedShort();
            if (count == 0 || count > 16) {
                throw new IOException("Invalid IPC field count");
            }
            String[] fields = new String[count];
            for (int i = 0; i < count; i++) {
                int length = input.readInt();
                if (length < 0 || length > input.available()) {
                    throw new IOException("Invalid IPC field length");
                }
                byte[] bytes = input.readNBytes(length);
                fields[i] = StandardCharsets.UTF_8.newDecoder().decode(ByteBuffer.wrap(bytes)).toString();
                Arrays.fill(bytes, (byte) 0);
            }
            if (input.available() != 0) {
                throw new IOException("Unexpected IPC trailing data");
            }
            return fields;
        }
    }

    private static void readFully(SocketChannel channel, ByteBuffer data) throws IOException {
        while (data.hasRemaining()) {
            if (channel.read(data) < 0) {
                throw new EOFException("Supervisor disconnected");
            }
        }
    }

    static String[] read(SocketChannel channel) throws IOException {
        ByteBuffer header = ByteBuffer.allocate(4);
        readFully(channel, header);
        int size = header.flip().getInt();
        if (size < 2 || size > LIMIT) {
            throw new IOException("Invalid IPC frame length");
        }
        byte[] bytes = new byte[size];
        readFully(channel, ByteBuffer.wrap(bytes));
        try {
            return decode(bytes);
        } finally {
            Arrays.fill(bytes, (byte) 0);
        }
    }

    static void write(SocketChannel channel, String[] fields) throws IOException {
        byte[] body = encode(fields);
        ByteBuffer buffer = ByteBuffer.allocate(body.length + 4).putInt(body.length).put(body);
        buffer.flip();
        while (buffer.hasRemaining()) {
            channel.write(buffer);
        }
        Arrays.fill(body, (byte) 0);
        Arrays.fill(buffer.array(), (byte) 0);
    }

    static void writeRestartRecord(String session) throws IOException {
        if (runtime == null || generation == null) {
            throw new IOException("Restart requested without supervisor ownership");
        }
        if (session == null || !session.matches("[A-Za-z0-9_-]{1,128}")) {
            throw new IOException("Unrecognized native restart identity");
        }
        String content = generation + "\n" + session + "\n";
        Path temporary = Files.createTempFile(runtime, "restart-", ".tmp",
                PosixFilePermissions.asFileAttribute(PosixFilePermissions.fromString("rw-------")));
        Files.writeString(temporary, content, StandardCharsets.UTF_8);
        Files.move(temporary, runtime.resolve("restart.request"),
                StandardCopyOption.ATOMIC_MOVE, StandardCopyOption.REPLACE_EXISTING);
    }

    private static void recordNativeShutdown() {
        if (shutdownRequested || AUTHORIZED.getCount() != 0 || runtime == null || ui == null || ui.readOnlySession) {
            return;
        }
        try (var directories = Files.list(runtime.resolve("settings"))) {
            List<String> sessions = new ArrayList<>();
            for (Path directory : directories.toList()) {
                Path token = directory.resolve("autorestart");
                if (Files.isDirectory(directory, LinkOption.NOFOLLOW_LINKS)
                        && Files.isRegularFile(token, LinkOption.NOFOLLOW_LINKS)
                        && Files.getLastModifiedTime(token, LinkOption.NOFOLLOW_LINKS).toMillis() >= bootTimestamp) {
                    sessions.add(directory.getFileName().toString());
                }
            }
            if (sessions.size() == 1) {
                writeRestartRecord(sessions.get(0));
            } else if (sessions.size() > 1) {
                throw new IOException("Ambiguous native restart session");
            }
        } catch (IOException error) {
            System.err.println("Gateway native restart journal could not be recorded");
        }
    }

    static List<Component> components(Component root) {
        ArrayList<Component> result = new ArrayList<>();
        ArrayList<Component> pending = new ArrayList<>();
        pending.add(root);
        for (int i = 0; i < pending.size(); i++) {
            if (pending.size() > 8192) {
                throw new IllegalStateException("UI component limit exceeded");
            }
            Component component = pending.get(i);
            result.add(component);
            if (component instanceof Container container) {
                pending.addAll(Arrays.asList(container.getComponents()));
            }
            if (component instanceof JMenu menu) {
                pending.add(menu.getPopupMenu());
            }
        }
        return result;
    }

    static String text(String value) {
        return value == null ? "" : value.replaceAll("<[^>]*>", " ").replaceAll("\\s+", " ").strip().replaceAll(":$", "");
    }

    static <T extends Component> List<T> matching(Component root, Class<T> type) {
        return components(root).stream().filter(type::isInstance).map(type::cast).toList();
    }

    static <T> T unique(List<T> values, String description) {
        if (values.size() != 1) {
            throw new UiFailure("unsupported_ui_" + description.replace(' ', '_'));
        }
        return values.get(0);
    }

    static AbstractButton button(Component root, String label) {
        return unique(matching(root, AbstractButton.class).stream()
                .filter(b -> text(b.getText()).equalsIgnoreCase(label)).toList(), "button");
    }

    static JTextField field(Component root, String label) {
        List<JLabel> labels = matching(root, JLabel.class).stream()
                .filter(l -> text(l.getText()).equalsIgnoreCase(label)).toList();
        if (labels.size() == 1 && labels.get(0).getLabelFor() instanceof JTextField f) {
            return f;
        }
        for (Component c : components(root)) {
            if (text(c.getName()).equalsIgnoreCase(label)) {
                return unique(matching(c, JTextField.class), "named field");
            }
        }
        JLabel l = unique(labels, "field label");
        Container parent = l.getParent();
        for (int depth = 0; depth < 2 && parent != null; depth++, parent = parent.getParent()) {
            List<JTextField> fields = matching(parent, JTextField.class);
            if (fields.size() == 1) {
                return fields.get(0);
            }
        }
        throw new UiFailure("unsupported_ui_labeled_field");
    }

    static boolean selectTree(Component root, String... names) {
        JTree tree = unique(matching(root, JTree.class), "configuration tree");
        TreeModel model = tree.getModel();
        Object node = model.getRoot();
        TreePath path = new TreePath(node);
        for (String name : names) {
            Object next = null;
            for (int i = 0; i < model.getChildCount(node); i++) {
                Object child = model.getChild(node, i);
                if (text(child.toString()).equalsIgnoreCase(name)) {
                    if (next != null) {
                        throw new IllegalStateException("Ambiguous configuration section");
                    }
                    next = child;
                }
            }
            if (next == null) {
                return false;
            }
            node = next;
            path = path.pathByAddingChild(node);
        }
        tree.setSelectionPath(path);
        tree.scrollPathToVisible(path);
        return true;
    }

    private static String title(Window window) {
        if (window instanceof JFrame frame) {
            return frame.getTitle();
        }
        if (window instanceof Dialog dialog) {
            return dialog.getTitle();
        }
        return "";
    }

    private static String labelText(Window window) {
        StringBuilder content = new StringBuilder();
        for (JLabel label : matching(window, JLabel.class)) {
            if (content.length() >= 8192) {
                break;
            }
            content.append(' ').append(text(label.getText()));
        }
        return content.toString().toLowerCase(Locale.ROOT);
    }

    static String dialogText(Component root) {
        StringBuilder result = new StringBuilder();
        for (Component component : components(root)) {
            String value = null;
            if (component instanceof JLabel label) {
                value = label.getText();
            } else if (component instanceof AbstractButton button) {
                value = "button: " + button.getText();
            } else if (component instanceof JTextArea area && !area.isEditable()) {
                value = area.getText();
            } else if (component instanceof JEditorPane pane && !pane.isEditable()) {
                value = pane.getText();
            }
            if (value != null) {
                result.append(text(value.substring(0, Math.min(value.length(), 4096)))).append('\n');
            }
            if (result.length() >= 8192) {
                break;
            }
        }
        return result.toString().replaceAll("\\b(?:DU|U|DF|F)[0-9]+\\b", "[account]")
                .replaceAll("\\b[0-9]{4,}\\b", "[number]");
    }

    private static boolean mfaPrompt(Window window) {
        String heading = title(window).toLowerCase(Locale.ROOT);
        return heading.contains("second factor") || labelText(window).contains("second factor authentication")
                || heading.contains("passkey") || heading.contains("security code");
    }

    private static final class UiFailure extends RuntimeException {
        private static final long serialVersionUID = 1L;
        UiFailure(String code) { super(code); }
    }

    private static final class Ui {
        private String mode;
        private int port;
        private LocalTime restartTime;
        private boolean orders;
        private boolean blocked;
        private boolean submitted;
        private boolean requestedCredentials;
        private boolean mfa;
        private volatile boolean readOnlySession;
        private boolean stopping;
        private boolean configured;
        private boolean writable;
        private boolean enableOrders;
        private int configStep;
        private Window configuration;
        private long submittedAt;
        private final long bootedAt = System.nanoTime();
        private long resumeGraceSecs;
        private long mfaTimeoutSecs;
        private long mfaSince;
        private long configTimeoutSecs;
        private long configProgressAt;
        private int observedConfigStep = -1;
        private long configAttempt;
        private boolean resumeExpected;
        private boolean nativeShutdown;

        void configure(String[] config) {
            if (!config[1].equals("paper") && !config[1].equals("live")) {
                throw new IllegalArgumentException("Invalid mode");
            }
            mode = config[1];
            port = Integer.parseInt(config[2]);
            if (port < 1024 || port > 65535) {
                throw new IllegalArgumentException("Invalid API port");
            }
            if (scheduledRestartEpoch == 0) {
                restartTime = LocalTime.parse(config[3]);
            }
            orders = Boolean.parseBoolean(config[4]);
            resumeGraceSecs = Long.parseLong(config[5]);
            mfaTimeoutSecs = Long.parseLong(config[6]);
            configTimeoutSecs = Long.parseLong(config[7]);
            resumeExpected = Boolean.parseBoolean(config[8]);
            if (resumeGraceSecs < 1 || mfaTimeoutSecs < 1 || configTimeoutSecs < 1) {
                throw new IllegalArgumentException("Invalid automation deadline");
            }
        }

        void resetConfiguration() {
            configured = false;
            configuration = null;
            configStep = 0;
            observedConfigStep = -1;
            configProgressAt = System.nanoTime();
            configAttempt++;
        }

        void clickForConfiguration(AbstractButton button) {
            long attempt = configAttempt;
            SwingUtilities.invokeLater(() -> {
                if (!blocked && attempt == configAttempt) {
                    button.doClick();
                }
            });
        }

        void requireFreshLogin() {
            blocked = true;
            configured = false;
            writable = false;
            boolean unchanged = state.equals("needs_attention")
                    && reason.equals("read_only_login_requires_fresh_session");
            emit("needs_attention", "read_only_login_requires_fresh_session");
            if (unchanged) {
                EVENTS.offer(new String[] {"STATE", state, reason});
            }
        }

        void command(String[] command) {
            switch (command[0]) {
                case "DIAGNOSE" -> {
                    for (Window window : Window.getWindows()) {
                        if (window.isShowing()) {
                            String shape = "window_class=" + window.getClass().getName()
                                    + "\nframe_menu_bar=" + (window instanceof JFrame frame && frame.getJMenuBar() != null)
                                    + "\nvisible_password_fields=" + matching(window, JPasswordField.class).stream()
                                            .filter(Component::isShowing).count()
                                    + "\nrecognized_main=" + isMain(window)
                                    + "\nautomation_blocked=" + blocked
                                    + "\nconfiguration_step=" + configStep
                                    + "\nconfiguration_verified=" + configured
                                    + "\nsettings_menu_enabled=" + matching(window, JMenuItem.class).stream()
                                            .filter(item -> text(item.getText()).equals("Settings"))
                                            .map(item -> Boolean.toString(item.isEnabled())).toList() + "\n";
                            EVENTS.offer(new String[] {"DIALOG", title(window), shape + dialogText(window)});
                        }
                    }
                }
                case "LOGIN" -> {
                    if (command.length != 3 || blocked || submitted || mfa) {
                        throw new IllegalStateException("Unexpected credential submission");
                    }
                    Window login = unique(loginWindows(), "Gateway login window");
                    List<JPasswordField> passwords = matching(login, JPasswordField.class).stream()
                            .filter(Component::isShowing).filter(Component::isEnabled).toList();
                    JPasswordField password = unique(passwords, "password field");
                    JTextField username = unique(matching(login, JTextField.class).stream()
                            .filter(f -> !(f instanceof JPasswordField))
                            .filter(Component::isShowing).filter(Component::isEnabled).toList(), "username field");
                    JToggleButton tradingMode = unique(matching(login, JToggleButton.class).stream()
                            .filter(b -> text(b.getText()).equalsIgnoreCase(mode.equals("live") ? "Live Trading" : "Paper Trading"))
                            .toList(), "trading mode");
                    if (!tradingMode.isSelected()) {
                        throw new UiFailure("login_mode_changed_before_submission");
                    }
                    username.setText(command[1]);
                    password.setText(command[2]);
                    AbstractButton submit = unique(matching(login, AbstractButton.class).stream()
                            .filter(b -> List.of("Login", "Log In", "Paper Log In").contains(text(b.getText())))
                            .filter(Component::isShowing).filter(Component::isEnabled).toList(), "login button");
                    submitted = true;
                    submittedAt = System.nanoTime();
                    emit("authenticating", "credentials_submitted");
                    SwingUtilities.invokeLater(submit::doClick);
                }
                case "ENABLE_ORDERS" -> {
                    if (readOnlySession) {
                        requireFreshLogin();
                        return;
                    }
                    if (!configured || !orders || blocked) {
                        throw new IllegalStateException("Live permission change without verified configuration");
                    }
                    enableOrders = true;
                    resetConfiguration();
                }
                case "RESTART" -> {
                    if (!configured || blocked || mfa) {
                        throw new IllegalStateException("Gateway is not ready for native restart");
                    }
                    ZonedDateTime scheduled = ZonedDateTime.now().plusMinutes(3).withSecond(0).withNano(0);
                    restartTime = scheduled.toLocalTime();
                    scheduledRestartEpoch = scheduled.toEpochSecond();
                    enableOrders = writable;
                    resetConfiguration();
                    emit("native_restarting", "supervisor_requested_session_preserving_restart");
                    if (!EVENTS.offer(new String[] {"RESTART_SCHEDULED", Long.toString(scheduledRestartEpoch)})) {
                        blocked = true;
                        emit("needs_attention", "restart_schedule_delivery_failed");
                    }
                }
                case "STOP" -> {
                    shutdownRequested = true;
                    stopping = true;
                    for (Window window : Window.getWindows()) {
                        if (isMain(window)) {
                            SwingUtilities.invokeLater(() ->
                                    window.dispatchEvent(new WindowEvent(window, WindowEvent.WINDOW_CLOSING)));
                            return;
                        }
                    }
                    System.exit(0);
                }
                case "RESUME" -> {
                    if (readOnlySession) {
                        requireFreshLogin();
                        return;
                    }
                    blocked = false;
                    enableOrders = false;
                    writable = false;
                    resetConfiguration();
                    mfa = Arrays.stream(Window.getWindows()).filter(Component::isShowing)
                            .anyMatch(GatewayBridge::mfaPrompt);
                    mfaSince = mfa ? System.nanoTime() : 0;
                    if (!mfa && !loginWindows().isEmpty()) {
                        submitted = false;
                        requestedCredentials = false;
                    }
                    String nextState = mfa ? "awaiting_mfa" : "starting";
                    String nextReason = mfa ? "user_approval_required" : "operator_resumed";
                    boolean unchanged = state.equals(nextState) && reason.equals(nextReason);
                    emit(nextState, nextReason);
                    if (unchanged) {
                        EVENTS.offer(new String[] {"STATE", state, reason});
                    }
                }
                default -> throw new IllegalArgumentException("Unknown bridge command");
            }
        }

        List<Window> loginWindows() {
            return Arrays.stream(Window.getWindows()).filter(Component::isShowing)
                    .filter(w -> title(w).contains("Gateway"))
                    .filter(w -> matching(w, JPasswordField.class).stream()
                            .anyMatch(Component::isShowing)).toList();
        }

        boolean isMain(Window window) {
            return window.isShowing() && window instanceof JFrame frame
                    && frame.getJMenuBar() != null && title(window).contains("Gateway")
                    && matching(window, JPasswordField.class).stream().noneMatch(Component::isShowing);
        }

        void tick() {
            if (mode == null || (blocked && !stopping)) {
                return;
            }
            boolean challengeVisible = false;
            for (Window window : Window.getWindows()) {
                if (!window.isShowing()) {
                    continue;
                }
                String content = labelText(window);
                String heading = title(window).toLowerCase(Locale.ROOT);
                if (stopping) {
                    if (window instanceof JDialog && (heading.contains("exit") || heading.contains("log out"))) {
                        AbstractButton confirm = button(window, "Yes");
                        SwingUtilities.invokeLater(confirm::doClick);
                    }
                    continue;
                }
                if (window instanceof JDialog && heading.equals("restart in progress")) {
                    nativeShutdown = true;
                    emit("native_restarting", "gateway_native_shutdown_in_progress");
                    continue;
                }
                boolean restartNotice = window instanceof JDialog && configuration != null
                        && (configStep == 3 || configStep == 4 || configStep == 7 || configStep == 8)
                        && dialogText(window).toLowerCase(Locale.ROOT).contains(
                                "you have elected to have your trading platform restart automatically on a daily basis");
                if (restartNotice) {
                    if (!"gatewayctl-restart-notice-accepted".equals(window.getName())) {
                        AbstractButton acknowledge = button(window, "OK");
                        if (acknowledge.isEnabled()) {
                            window.setName("gatewayctl-restart-notice-accepted");
                            clickForConfiguration(acknowledge);
                        }
                    }
                    return;
                }
                boolean paperWarning = window instanceof JDialog
                        && matching(window, JLabel.class).stream()
                                .anyMatch(label -> text(label.getText()).toLowerCase(Locale.ROOT)
                                        .startsWith("this is not a brokerage account"));
                if (paperWarning) {
                    if (!mode.equals("paper")) {
                        blocked = true;
                        emit("needs_attention", "non_brokerage_account_warning_in_live_mode");
                        return;
                    }
                    if (!"gatewayctl-paper-warning-accepted".equals(window.getName())) {
                        AbstractButton accept = button(window, "I understand and accept");
                        if (accept.isEnabled()) {
                            window.setName("gatewayctl-paper-warning-accepted");
                            SwingUtilities.invokeLater(accept::doClick);
                        }
                    }
                    return;
                }
                if (mfaPrompt(window)) {
                    if (!mfa) {
                        mfaSince = System.nanoTime();
                    }
                    mfa = true;
                    challengeVisible = true;
                    for (AbstractButton choice : matching(window, AbstractButton.class)) {
                        if (text(choice.getText()).equalsIgnoreCase("Enter Read Only")
                                && choice.getClientProperty("gatewayctl-readonly-observer") == null) {
                            choice.putClientProperty("gatewayctl-readonly-observer", Boolean.TRUE);
                            choice.addActionListener(event -> {
                                readOnlySession = true;
                                blocked = true;
                                configured = false;
                                writable = false;
                                emit("needs_attention", "read_only_login_cannot_provide_trading");
                            });
                        }
                        if (System.nanoTime() - mfaSince >= TimeUnit.SECONDS.toNanos(mfaTimeoutSecs)) {
                            blocked = true;
                            emit("needs_attention", "mfa_not_completed");
                            return;
                        }
                    }
                    emit("awaiting_mfa", "user_approval_required");
                    continue;
                }
                if (heading.contains("login failed") && (resumeExpected || !System.getProperty("restart", "").isEmpty())) {
                    emit("resume_login_required", "native_session_requires_full_login");
                    return;
                }
                if (heading.contains("login failed") || heading.contains("existing session")
                        || heading.contains("too many failed") || content.contains("invalid username or password")) {
                    blocked = true;
                    emit("needs_attention", "authentication_or_session_conflict");
                    return;
                }
                if (window instanceof JDialog dialog && dialog.isModal()
                        && window != configuration
                        && !(heading.contains("configuration") && !matching(window, JTree.class).isEmpty())
                        && !heading.contains("second factor") && !heading.contains("security code")
                        && !heading.contains("passkey")) {
                    blocked = true;
                    EVENTS.offer(new String[] {"DIALOG", title(window), dialogText(window)});
                    emit("needs_attention", "unrecognized_modal_dialog");
                    return;
                }
            }
            if (challengeVisible || stopping || nativeShutdown) {
                return;
            }
            List<Window> main = Arrays.stream(Window.getWindows()).filter(this::isMain).toList();
            if (!main.isEmpty()) {
                mfa = false;
                if (!configured) {
                    if (observedConfigStep != configStep) {
                        observedConfigStep = configStep;
                        configProgressAt = System.nanoTime();
                    } else if (System.nanoTime() - configProgressAt >= TimeUnit.SECONDS.toNanos(configTimeoutSecs)) {
                        throw new UiFailure("configuration_stalled");
                    }
                    configureGateway(unique(main, "Gateway main window"));
                } else {
                    emit(writable ? "configured_writable" : "configured_readonly", "settings_verified");
                }
                return;
            }
            List<Window> logins = loginWindows();
            if (mfa && logins.stream().anyMatch(window ->
                    matching(window, JPasswordField.class).stream()
                            .anyMatch(field -> field.isShowing() && field.isEnabled()))) {
                blocked = true;
                emit("needs_attention", "mfa_cancelled_or_expired");
                return;
            }
            if (!mfa && !submitted && !requestedCredentials && logins.size() == 1) {
                List<JToggleButton> api = matching(logins.get(0), JToggleButton.class).stream()
                        .filter(b -> List.of("IB API", "TWS/API").contains(text(b.getText()))).toList();
                if (api.size() != 1) {
                    throw new IllegalStateException("Missing API mode selector");
                }
                if (!api.get(0).isSelected()) {
                    SwingUtilities.invokeLater(api.get(0)::doClick);
                    return;
                }
                JToggleButton tradingMode = unique(matching(logins.get(0), JToggleButton.class).stream()
                        .filter(b -> text(b.getText()).equalsIgnoreCase(
                                mode.equals("live") ? "Live Trading" : "Paper Trading"))
                        .toList(), "trading mode");
                if (!tradingMode.isSelected()) {
                    SwingUtilities.invokeLater(tradingMode::doClick);
                    return;
                }
                if (resumeExpected || !System.getProperty("restart", "").isEmpty()) {
                    if (System.nanoTime() - bootedAt >= TimeUnit.SECONDS.toNanos(resumeGraceSecs)) {
                        emit("resume_login_required", "native_session_requires_full_login");
                    }
                    return;
                }
                requestedCredentials = true;
                emit("login_required", "credential_fields_ready");
            } else if (submitted && !mfa
                    && System.nanoTime() - submittedAt > TimeUnit.SECONDS.toNanos(90)) {
                blocked = true;
                emit("needs_attention", "login_did_not_complete");
            }
        }

        void configureGateway(Window main) {
            if (configStep == 4 || configStep == 8) {
                if (configuration != null && configuration.isShowing()) {
                    return;
                }
                configuration = null;
                if (configStep == 8) {
                    if (!blocked && !readOnlySession) {
                        configured = true;
                        writable = enableOrders;
                        emit(writable ? "configured_writable" : "configured_readonly", "settings_verified");
                    }
                    return;
                }
            }
            if (configuration == null || !configuration.isShowing()) {
                List<Window> existing = Arrays.stream(Window.getWindows())
                        .filter(Component::isShowing).filter(w -> title(w).contains("Configuration")).toList();
                if (existing.size() == 1 && (configStep == 1 || configStep == 5)) {
                    configuration = existing.get(0);
                    configuration.setName("gatewayctl-owned-configuration");
                } else if (existing.size() == 1 && configStep == 0
                        && "gatewayctl-owned-configuration".equals(existing.get(0).getName())) {
                    configuration = existing.get(0);
                    configStep = 1;
                } else if (!existing.isEmpty()) {
                    throw new UiFailure("configuration_dialog_requires_user_resolution");
                } else if (configStep == 0 || configStep == 4) {
                    JMenu menu = unique(matching(main, JMenu.class).stream()
                            .filter(m -> text(m.getText()).equals("Configure")).toList(), "Configure menu");
                    JMenuItem settings = unique(matching(menu, JMenuItem.class).stream()
                            .filter(m -> text(m.getText()).equals("Settings")).toList(), "Settings menu item");
                    if (!settings.isEnabled()) {
                        return;
                    }
                    int nextStep = configStep == 0 ? 1 : 5;
                    long attempt = configAttempt;
                    SwingUtilities.invokeLater(() -> {
                        if (!blocked && attempt == configAttempt && settings.isEnabled()) {
                            configStep = nextStep;
                            settings.doClick();
                        }
                    });
                    return;
                } else {
                    return;
                }
            }
            if (configStep == 1 || configStep == 5) {
                if (!selectTree(configuration, "API", "Settings")) {
                    throw new IllegalStateException("Unsupported API settings tree");
                }
                configStep = configStep == 1 ? 2 : 6;
                return;
            }
            if (configStep == 2 || configStep == 6) {
                JTextField socket = field(configuration, "Socket port");
                JCheckBox readOnly = unique(matching(configuration, JCheckBox.class).stream()
                        .filter(c -> text(c.getText()).equals("Read-Only API")).toList(), "Read-Only API");
                List<JCheckBox> local = matching(configuration, JCheckBox.class).stream()
                        .filter(c -> text(c.getText()).equals("Allow connections from localhost only")).toList();
                if (local.size() != 1) {
                    throw new IllegalStateException("Localhost-only API control not found");
                }
                if (configStep == 2) {
                    socket.setText(Integer.toString(port));
                    if (readOnly.isSelected() == enableOrders) {
                        clickForConfiguration(readOnly);
                        return;
                    }
                    if (!local.get(0).isSelected()) {
                        clickForConfiguration(local.get(0));
                        return;
                    }
                } else if (!socket.getText().strip().equals(Integer.toString(port))
                        || readOnly.isSelected() == enableOrders || !local.get(0).isSelected()) {
                    throw new UiFailure("api_settings_readback_failed");
                }
                if (!selectTree(configuration, "Lock and Exit")) {
                    throw new IllegalStateException("Auto-restart controls unavailable");
                }
                configStep = configStep == 2 ? 3 : 7;
                return;
            }
            if (configStep == 3 || configStep == 7) {
                List<AbstractButton> restartChoices = matching(configuration, AbstractButton.class).stream()
                        .filter(choice -> text(choice.getText()).equalsIgnoreCase("Auto restart")).toList();
                if (restartChoices.isEmpty()) {
                    throw new UiFailure("native_auto_restart_setting_unavailable");
                }
                AbstractButton auto = unique(restartChoices, "auto_restart");
                List<JLabel> labels = matching(configuration, JLabel.class).stream()
                        .filter(l -> text(l.getText()).startsWith("Set Auto Restart Time")
                                || text(l.getText()).startsWith("Set Auto Log Off Time")).toList();
                Container section = unique(labels, "auto restart label").getParent().getParent();
                JTextField time = unique(matching(section, JTextField.class), "restart time");
                String expectedTime = restartTime.format(DateTimeFormatter.ofPattern("hh:mm", Locale.ROOT));
                AbstractButton halfDay = button(section, restartTime.getHour() < 12 ? "AM" : "PM");
                if (configStep == 3) {
                    if (!auto.isSelected()) {
                        clickForConfiguration(auto);
                        return;
                    }
                    time.setText(expectedTime);
                    if (!halfDay.isSelected()) {
                        clickForConfiguration(halfDay);
                        return;
                    }
                } else if (!auto.isSelected() || !time.getText().strip().equals(expectedTime) || !halfDay.isSelected()) {
                    throw new UiFailure("restart_settings_readback_failed");
                }
                boolean verified = configStep == 7;
                configStep = verified ? 8 : 4;
                clickForConfiguration(button(configuration, "OK"));
            }
        }
    }
}
