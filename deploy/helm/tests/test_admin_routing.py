"""Render operator deployment variants. Run with Python 3.11+, Helm, and yq v4."""

import itertools
import json
from pathlib import Path
import subprocess
import tomllib
import unittest


CHARTS = Path(__file__).resolve().parents[1]
TOKEN = "0123456789abcdef0123456789abcdef"


def render(chart, *values):
    command = ["helm", "template", chart, str(CHARTS / chart)]
    for value in values:
        command.extend(["--set-string", value])
    # Helm needs real boolean values for conditionals.
    command = [
        "--set" if item == "--set-string" and command[index + 1].endswith(("=true", "=false")) else item
        for index, item in enumerate(command)
    ]
    rendered = subprocess.check_output(command, text=True)
    documents = subprocess.check_output(
        ["yq", "-o=json", "-I=0", "."], input=rendered, text=True
    )
    return [document for line in documents.splitlines() if (document := json.loads(line))]


class AdminRoutingTests(unittest.TestCase):
    def test_coordinator_service_isolation_and_token_mounts(self):
        for blue_green, external, public_type in itertools.product(
            [False, True], [False, True], ["ClusterIP", "NodePort", "LoadBalancer"]
        ):
            with self.subTest(blue_green=blue_green, external=external, public_type=public_type):
                documents = render(
                    "coordinator",
                    f"blueGreen.enabled={str(blue_green).lower()}",
                    "blueGreen.activeSlot=green",
                    f"service.type={public_type}",
                    f"secrets.adminToken.external={str(external).lower()}",
                    f"secrets.adminToken.create={str(not external).lower()}",
                    "secrets.adminToken.secretName=existing-operator-token",
                    f"secrets.adminToken.value={TOKEN}",
                )
                services = {
                    document["metadata"]["name"]: document["spec"]
                    for document in documents if document["kind"] == "Service"
                }
                public, admin = services["coordinator"], services["coordinator-admin"]
                self.assertEqual(public["type"], public_type)
                self.assertEqual([port["targetPort"] for port in public["ports"]], ["http"])
                self.assertEqual(admin["type"], "ClusterIP")
                self.assertEqual([port["targetPort"] for port in admin["ports"]], ["admin"])
                self.assertEqual(public["selector"], admin["selector"])
                if blue_green:
                    self.assertEqual(admin["selector"]["app.kubernetes.io/slot"], "green")

                deployments = [d for d in documents if d["kind"] == "Deployment"]
                self.assertEqual(len(deployments), 2 if blue_green else 1)
                expected_secret = "existing-operator-token" if external else "coordinator-admin-token"
                for deployment in deployments:
                    pod = deployment["spec"]["template"]["spec"]
                    volumes = {volume["name"]: volume for volume in pod["volumes"]}
                    self.assertEqual(volumes["admin-token"]["secret"]["secretName"], expected_secret)
                    coordinator = next(c for c in pod["containers"] if c["name"] == "coordinator")
                    mounts = {mount["name"]: mount for mount in coordinator["volumeMounts"]}
                    self.assertEqual(mounts["admin-token"]["mountPath"], "/etc/coordinator-admin")

                configmap = next(d for d in documents if d["kind"] == "ConfigMap" and d["metadata"]["name"] == "coordinator")
                config = tomllib.loads(configmap["data"]["local.toml"])
                self.assertEqual(config["admin_settings"]["token_file"], "/etc/coordinator-admin/token")

    def test_maintenance_upgrade_disables_only_the_switchover(self):
        values = (
            "blueGreen.enabled=true",
            "blueGreen.activeSlot=green",
            "secrets.adminToken.external=true",
            "secrets.adminToken.secretName=existing-operator-token",
        )
        normal = render("coordinator", *values)
        maintenance = render("coordinator", *values, "blueGreen.switchoverEnabled=false")
        hook_names = {"coordinator-bg-script", "coordinator-bg-switchover"}
        self.assertEqual(
            {d["metadata"]["name"] for d in normal} & hook_names, hook_names
        )
        self.assertEqual(
            maintenance, [d for d in normal if d["metadata"]["name"] not in hook_names]
        )
        deployments = [d for d in maintenance if d["kind"] == "Deployment"]
        self.assertEqual(
            {d["metadata"]["name"]: d["spec"]["replicas"] for d in deployments},
            {"coordinator-blue": 0, "coordinator-green": 1},
        )
        self.assertEqual(
            {d["metadata"]["name"] for d in maintenance if d["kind"] == "PersistentVolumeClaim"},
            {"coordinator-blue", "coordinator-green"},
        )
        admin = next(d for d in maintenance if d["metadata"]["name"] == "coordinator-admin")
        self.assertEqual(admin["spec"]["selector"]["app.kubernetes.io/slot"], "green")
        for deployment in deployments:
            pod = deployment["spec"]["template"]["spec"]
            volume = next(v for v in pod["volumes"] if v["name"] == "admin-token")
            self.assertEqual(volume["secret"]["secretName"], "existing-operator-token")
            coordinator = next(c for c in pod["containers"] if c["name"] == "coordinator")
            mount = next(m for m in coordinator["volumeMounts"] if m["name"] == "admin-token")
            self.assertEqual(mount["mountPath"], "/etc/coordinator-admin")

    def test_synth_uses_operator_service_and_secret(self):
        documents = render("synth", "config.coordinator.adminTokenSecret.enabled=true")
        configmap = next(d for d in documents if d["kind"] == "ConfigMap")
        config = tomllib.loads(next(iter(configmap["data"].values())))
        self.assertEqual(config["coordinator"]["admin_url"], "http://coordinator-admin.coordinator.svc.cluster.local:9991")
        self.assertEqual(config["coordinator"]["admin_token_file"], "/etc/synth-secrets/admin-token")
        deployment = next(d for d in documents if d["kind"] == "Deployment")
        volume = next(v for v in deployment["spec"]["template"]["spec"]["volumes"] if v["name"] == "admin-token")
        self.assertEqual(volume["secret"]["items"], [{"key": "token", "path": "admin-token"}])


if __name__ == "__main__":
    unittest.main()
