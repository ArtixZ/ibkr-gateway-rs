package ibgateway;

import java.awt.BorderLayout;
import java.awt.GridLayout;
import java.awt.event.WindowAdapter;
import java.awt.event.WindowEvent;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.atomic.AtomicInteger;
import javax.swing.ButtonGroup;
import javax.swing.JButton;
import javax.swing.JCheckBox;
import javax.swing.JDialog;
import javax.swing.JEditorPane;
import javax.swing.JFrame;
import javax.swing.JLabel;
import javax.swing.JMenu;
import javax.swing.JMenuBar;
import javax.swing.JMenuItem;
import javax.swing.JPanel;
import javax.swing.JPasswordField;
import javax.swing.JRadioButton;
import javax.swing.JTextField;
import javax.swing.JToggleButton;
import javax.swing.JTree;
import javax.swing.SwingUtilities;
import javax.swing.Timer;
import javax.swing.tree.DefaultMutableTreeNode;

/** Synthetic UI only. This class is never included in the production bridge JAR. */
public final class GWClient {
    private static String port = "9999";
    private static String time = "12:00";
    private static boolean pm;
    private static boolean readonly = true;
    private static boolean localhost;
    private static boolean restart;
    private static Path settings;
    private static final AtomicInteger SAVES = new AtomicInteger();
    private static boolean validationShown;
    private static boolean restartNoticeShown;
    private static JDialog connectionNotice;
    private static JDialog versionNotice;

    private GWClient() {}

    public static void main(String[] args) throws Exception {
        if (!Boolean.getBoolean("gatewayctl.fixture")) {
            throw new IllegalStateException("Synthetic UI requires an explicit fixture marker");
        }
        settings = Path.of(args[0]);
        Files.writeString(settings.resolve("fixture-started"), "synthetic-only");
        SwingUtilities.invokeAndWait(() -> {
            if (!System.getProperty("restart", "").isEmpty()) {
                mainWindow();
            } else {
                login();
            }
        });
    }

    private static void login() {
        JFrame frame = new JFrame("IB Gateway fixture login");
        frame.setDefaultCloseOperation(JFrame.EXIT_ON_CLOSE);
        JPanel panel = new JPanel(new GridLayout(0, 1));
        JToggleButton api = new JToggleButton("IB API", true);
        JToggleButton live = new JToggleButton("Live Trading", true);
        JToggleButton paper = new JToggleButton("Paper Trading");
        ButtonGroup modes = new ButtonGroup();
        modes.add(live);
        modes.add(paper);
        JTextField username = new JTextField();
        JPasswordField password = new JPasswordField();
        JButton login = new JButton("Log In");
        login.addActionListener(event -> {
            boolean correctMode = System.getProperty("gatewayctl.fixture.mode", "paper").equals("live")
                    ? live.isSelected() : paper.isSelected();
            if (!username.getText().equals("fixture-user")
                    || !new String(password.getPassword()).equals("fixture-secret")
                    || !correctMode || !api.isSelected()) {
                throw new IllegalStateException("Wrong fixture credentials or trading mode");
            }
            frame.dispose();
            mainWindow();
        });
        for (java.awt.Component c : new java.awt.Component[] {api, live, paper, username, password, login}) {
            panel.add(c);
        }
        frame.add(panel);
        frame.setSize(300, 260);
        frame.setVisible(true);
    }

    private static void mainWindow() {
        JFrame frame = new JFrame("IB Gateway fixture");
        frame.setDefaultCloseOperation(JFrame.DO_NOTHING_ON_CLOSE);
        frame.addWindowListener(new WindowAdapter() {
            @Override public void windowClosing(WindowEvent event) {
                JDialog confirm = new JDialog(frame, "Exit Gateway", true);
                JButton yes = new JButton("Yes");
                yes.addActionListener(ignored -> System.exit(0));
                confirm.add(yes);
                confirm.setSize(200, 100);
                confirm.setVisible(true);
            }
        });
        JMenuBar bar = new JMenuBar();
        JMenu configure = new JMenu("Configure");
        JMenuItem settingsItem = new JMenuItem("Settings");
        settingsItem.setEnabled(false);
        Timer initialization = new Timer(3000, event -> settingsItem.setEnabled(true));
        initialization.setRepeats(false);
        initialization.start();
        settingsItem.addActionListener(event -> configuration(frame));
        configure.add(settingsItem);
        bar.add(configure);
        frame.setJMenuBar(bar);
        frame.add(new JLabel("Synthetic Gateway: no broker connection"));
        JPasswordField retainedLoginField = new JPasswordField();
        retainedLoginField.setVisible(false);
        frame.add(retainedLoginField, BorderLayout.SOUTH);
        frame.setSize(350, 120);
        frame.setVisible(true);
        Timer connectionEvents = new Timer(100, event -> {
            for (String kind : new String[] {"relogin", "notice", "conflict"}) {
                Path trigger = settings.resolve("fixture-connection-" + kind);
                if (Files.exists(trigger)) {
                    try {
                        Files.delete(trigger);
                    } catch (IOException error) {
                        throw new IllegalStateException("Cannot consume fixture connection event", error);
                    }
                    SwingUtilities.invokeLater(() -> connectionDialog(frame, kind));
                }
            }
            for (String kind : new String[] {"notice", "required"}) {
                Path trigger = settings.resolve("fixture-version-" + kind);
                if (Files.exists(trigger)) {
                    try {
                        Files.delete(trigger);
                    } catch (IOException error) {
                        throw new IllegalStateException("Cannot consume fixture version event", error);
                    }
                    SwingUtilities.invokeLater(() -> versionDialog(frame, kind.equals("required")));
                }
            }
            if (!frame.isDisplayable()) {
                ((Timer) event.getSource()).stop();
            }
        });
        connectionEvents.start();
        boolean paper = System.getProperty("gatewayctl.fixture.mode", "paper").equals("paper");
        if (paper) {
            SwingUtilities.invokeLater(() -> {
                JDialog warning = new JDialog(frame, "Paper trading notice", true);
                warning.add(new JLabel("<html><b>This is not a brokerage account</b>.<br>Simulated trading only.</html>"),
                        BorderLayout.CENTER);
                JButton accept = new JButton("I understand and accept");
                accept.addActionListener(event -> warning.dispose());
                warning.add(accept, BorderLayout.SOUTH);
                warning.setSize(350, 120);
                warning.setVisible(true);
            });
        } else if (Boolean.getBoolean("gatewayctl.fixture.mfa") && System.getProperty("restart", "").isEmpty()) {
                JDialog challenge = new JDialog(frame, "Second Factor Authentication", true);
                challenge.add(new JLabel("SECOND FACTOR AUTHENTICATION"), BorderLayout.CENTER);
                JButton readOnly = new JButton("Enter Read Only");
                readOnly.addActionListener(event -> challenge.dispose());
                challenge.add(readOnly, BorderLayout.SOUTH);
                Timer simulatedUser = new Timer(100, event -> {
                    if (Files.exists(settings.resolve("fixture-user-approved"))) {
                        ((Timer) event.getSource()).stop();
                        challenge.dispose();
                    } else if (Files.exists(settings.resolve("fixture-user-readonly"))) {
                        ((Timer) event.getSource()).stop();
                        readOnly.doClick();
                    } else if (Files.exists(settings.resolve("fixture-user-cancelled"))) {
                        ((Timer) event.getSource()).stop();
                        challenge.dispose();
                        frame.dispose();
                        login();
                    }
                });
                simulatedUser.start();
                challenge.setSize(350, 120);
                challenge.setVisible(true);
                simulatedUser.stop();
        }
        if (Boolean.getBoolean("gatewayctl.fixture.version-notice")) {
            SwingUtilities.invokeLater(() -> versionDialog(frame, false));
        }
    }

    private static void versionDialog(JFrame owner, boolean required) {
        if (!required && versionNotice != null) {
            versionNotice.setVisible(true);
            return;
        }
        JDialog dialog = new JDialog(owner, "IBKR Gateway", true);
        if (!required) {
            versionNotice = dialog;
        }
        String message = required
                ? "This version is no longer supported. You must upgrade before logging in."
                : "The version of the application you are running, 1044.1, needs to be upgraded, "
                        + "as it will be desupported on 20990101. "
                        + "The minimum supported version at that time will be 1050.1. "
                        + "The new version can be downloaded <a href=\"https://example.invalid/\">here</a>.";
        JEditorPane content = new JEditorPane("text/html", "<html>" + message + "</html>");
        content.setEditable(false);
        dialog.add(content, BorderLayout.CENTER);
        JButton ok = new JButton("OK");
        ok.addActionListener(event -> {
            try {
                Files.writeString(settings.resolve(
                        "fixture-version-" + (required ? "required" : "notice") + "-clicked"), "clicked");
            } catch (IOException error) {
                throw new IllegalStateException("Cannot record fixture version action", error);
            }
            SwingUtilities.invokeLater(dialog::dispose);
        });
        dialog.add(ok, BorderLayout.SOUTH);
        dialog.setSize(450, 200);
        dialog.setVisible(true);
    }

    private static void connectionDialog(JFrame owner, String kind) {
        if (kind.equals("notice") && connectionNotice != null) {
            connectionNotice.setVisible(true);
            return;
        }
        String title = switch (kind) {
            case "relogin" -> "Re-login is required";
            case "conflict" -> "Existing session detected";
            default -> "IBKR Gateway";
        };
        JDialog dialog = new JDialog(owner, title, true);
        if (kind.equals("notice")) {
            connectionNotice = dialog;
        }
        String message = switch (kind) {
            case "relogin" -> "Your connection was lost. Would you like to re-login?";
            case "conflict" -> "Another session with the same user name already exists. Would you like to login and disconnect the other session?";
            default -> "Connection to server failed: Server disconnected, please try again";
        };
        dialog.add(new JLabel(message), BorderLayout.CENTER);
        JPanel actions = new JPanel();
        String action = switch (kind) {
            case "relogin" -> "Re-login";
            case "conflict" -> "Reconnect This Session";
            default -> "OK";
        };
        JButton button = new JButton(action);
        button.addActionListener(event -> {
            try {
                Files.writeString(settings.resolve("fixture-connection-" + kind + "-clicked"), "clicked");
            } catch (IOException error) {
                throw new IllegalStateException("Cannot record fixture connection action", error);
            }
            SwingUtilities.invokeLater(dialog::dispose);
        });
        actions.add(button);
        if (!kind.equals("notice")) {
            JButton cancel = new JButton("Cancel");
            cancel.addActionListener(event -> dialog.dispose());
            actions.add(cancel);
        }
        dialog.add(actions, BorderLayout.SOUTH);
        dialog.setSize(450, 140);
        dialog.setVisible(true);
    }

    private static void configuration(JFrame owner) {
        try {
            Files.writeString(settings.resolve("fixture-configuration-opened"), "opened");
        } catch (IOException error) {
            throw new IllegalStateException("Cannot record fixture configuration", error);
        }
        if (Boolean.getBoolean("gatewayctl.fixture.config-stall")) {
            return;
        }
        JDialog dialog = new JDialog(owner, "Global Configuration", true);
        DefaultMutableTreeNode root = new DefaultMutableTreeNode("root");
        DefaultMutableTreeNode api = new DefaultMutableTreeNode("API");
        api.add(new DefaultMutableTreeNode("Settings"));
        root.add(api);
        root.add(new DefaultMutableTreeNode("Lock and Exit"));
        JTree tree = new JTree(root);
        JPanel content = new JPanel(new GridLayout(0, 1));
        JPanel apiPanel = new JPanel(new GridLayout(0, 1));
        JLabel socketLabel = new JLabel("Socket port:");
        JTextField socket = new JTextField(port);
        socketLabel.setLabelFor(socket);
        JCheckBox readOnly = new JCheckBox("Read-Only API", readonly);
        JCheckBox local = new JCheckBox("Allow connections from localhost only", localhost);
        apiPanel.add(socketLabel);
        apiPanel.add(socket);
        apiPanel.add(readOnly);
        apiPanel.add(local);
        JPanel restartPanel = new JPanel(new GridLayout(0, 1));
        JPanel labelPanel = new JPanel();
        labelPanel.add(new JLabel("Set Auto Restart Time (HH:MM)"));
        restartPanel.add(labelPanel);
        JTextField restartTime = new JTextField(time);
        JRadioButton auto = new JRadioButton("Auto restart", restart);
        JRadioButton am = new JRadioButton("AM", !pm);
        JRadioButton evening = new JRadioButton("PM", pm);
        ButtonGroup halves = new ButtonGroup();
        halves.add(am);
        halves.add(evening);
        restartPanel.add(restartTime);
        restartPanel.add(auto);
        restartPanel.add(am);
        restartPanel.add(evening);
        content.add(apiPanel);
        content.add(restartPanel);
        tree.addTreeSelectionListener(event -> {
            String selected = tree.getLastSelectedPathComponent().toString();
            apiPanel.setVisible(selected.equals("Settings"));
            restartPanel.setVisible(selected.equals("Lock and Exit"));
        });
        JButton ok = new JButton("OK");
        ok.addActionListener(event -> {
            if (auto.isSelected() && !restartNoticeShown) {
                restartNoticeShown = true;
                JDialog notice = new JDialog(dialog, "IBKR Gateway", true);
                JEditorPane message = new JEditorPane("text/html",
                        "<html>You have elected to have your trading platform restart automatically on a daily basis. "
                        + "There are some considerations you would need to take into account.</html>");
                message.setEditable(false);
                notice.add(message, BorderLayout.CENTER);
                JButton acknowledge = new JButton("OK");
                acknowledge.addActionListener(ignored -> notice.dispose());
                notice.add(acknowledge, BorderLayout.SOUTH);
                notice.setSize(450, 180);
                notice.setVisible(true);
            }
            if (Boolean.getBoolean("gatewayctl.fixture.config-error") && !validationShown) {
                validationShown = true;
                JDialog validation = new JDialog(dialog, "Validation notice", true);
                validation.add(new JLabel("Synthetic validation notice"));
                Timer simulatedUser = new Timer(100, ignored -> {
                    if (Files.exists(settings.resolve("fixture-dismiss-config-error"))) {
                        ((Timer) ignored.getSource()).stop();
                        validation.dispose();
                    }
                });
                simulatedUser.start();
                validation.setSize(300, 100);
                validation.setVisible(true);
                simulatedUser.stop();
                try {
                    Files.writeString(settings.resolve("fixture-config-error-dismissed"), "dismissed");
                } catch (IOException error) {
                    throw new IllegalStateException("Cannot record notice dismissal", error);
                }
                return;
            }
            port = socket.getText();
            readonly = readOnly.isSelected();
            localhost = local.isSelected();
            time = restartTime.getText();
            pm = evening.isSelected();
            restart = auto.isSelected();
            try {
                Files.writeString(settings.resolve("fixture-settings"),
                        port + "\n" + readonly + "\n" + localhost + "\n" + time + "\n" + pm + "\n" + restart + "\n");
            } catch (IOException error) {
                throw new IllegalStateException("Cannot save fixture", error);
            }
            dialog.dispose();
            if (SAVES.incrementAndGet() == 6) {
                JDialog progress = new JDialog(owner, "Restart in progress", true);
                progress.add(new JLabel("Preparing to shutdown IA..."));
                Timer nativeRestart = new Timer(2000, ignored -> {
                    try {
                        Files.createDirectories(settings.resolve("fixture-session"));
                        Files.writeString(settings.resolve("fixture-session/autorestart"), "not-a-real-token");
                        System.exit(0);
                    } catch (Exception error) {
                        throw new IllegalStateException("Synthetic native restart failed", error);
                    }
                });
                nativeRestart.setRepeats(false);
                nativeRestart.start();
                progress.setSize(350, 120);
                progress.setVisible(true);
            }
        });
        dialog.add(tree, BorderLayout.WEST);
        dialog.add(content, BorderLayout.CENTER);
        dialog.add(ok, BorderLayout.SOUTH);
        dialog.setSize(600, 400);
        dialog.setVisible(true);
    }
}
