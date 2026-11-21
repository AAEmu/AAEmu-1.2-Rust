param(
    [Parameter(Mandatory = $true)]
    [string]$ClientDirectory
)

$ErrorActionPreference = 'Stop'
$database = (Get-Item -LiteralPath (Join-Path $ClientDirectory 'game/db/compact.sqlite3')).FullName

if (-not ('AaemuShipSkillPatch' -as [type])) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class AaemuShipSkillPatch {
    [DllImport("winsqlite3.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int sqlite3_open_v2(byte[] path, out IntPtr db, int flags, IntPtr vfs);
    [DllImport("winsqlite3.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int sqlite3_exec(IntPtr db, byte[] sql, IntPtr callback, IntPtr arg, IntPtr error);
    [DllImport("winsqlite3.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern IntPtr sqlite3_errmsg(IntPtr db);
    [DllImport("winsqlite3.dll", CallingConvention = CallingConvention.Cdecl)]
    private static extern int sqlite3_close(IntPtr db);

    public static void Apply(string path, string sql) {
        IntPtr db;
        int result = sqlite3_open_v2(Encoding.UTF8.GetBytes(path + "\0"), out db, 2, IntPtr.Zero);
        try {
            if (result != 0) throw new InvalidOperationException("Could not open the client database for writing.");
            result = sqlite3_exec(db, Encoding.UTF8.GetBytes(sql + "\0"), IntPtr.Zero, IntPtr.Zero, IntPtr.Zero);
            if (result != 0) throw new InvalidOperationException(Marshal.PtrToStringAnsi(sqlite3_errmsg(db)));
        } finally {
            if (db != IntPtr.Zero) sqlite3_close(db);
        }
    }
}
'@
}

[AaemuShipSkillPatch]::Apply($database, @'
BEGIN IMMEDIATE;

-- Validate both the original and the patched forms. Closing the connection
-- rolls back an incomplete transaction if the database layout differs.
CREATE TEMP TABLE skill_patch_check (ok INTEGER NOT NULL CHECK (ok = 1));
INSERT INTO skill_patch_check SELECT COUNT(*) = 1 FROM skills
  WHERE id = 15660 AND target_type_id = 8 AND link_backpack_type_id = 1;
INSERT INTO skill_patch_check SELECT COUNT(*) = 2 FROM skill_reqs
  WHERE (id = 13 AND buff_tag_id = 306) OR (id = 35 AND buff_tag_id = 367);
INSERT INTO skill_patch_check SELECT COUNT(*) = 2 FROM skills
  WHERE (id = 17737 AND plot_id = 355 OR id = 17738 AND plot_id = 356)
    AND target_type_id IN (0, 11);
INSERT INTO skill_patch_check SELECT COUNT(*) = 8 FROM skills
  WHERE id IN (13608, 13609, 17649, 17949, 17950, 17957, 17958, 20704)
    AND source_stun IN ('t', 'f');

-- These requirements treat the glider's equipment buffs as a carried pack.
-- The server checks the actual backpack slot before it equips the cannon.
DELETE FROM skill_req_skills WHERE skill_id = 15660 AND skill_req_id IN (13, 35);

-- Select an impact point through the native cannon targeting UI. The projectile
-- has its own collision checks, so a straight obstacle ray cannot reject the arc.
UPDATE skills SET target_type_id = 11, target_selection_id = 2, min_range = 4, max_range = 45,
                  target_alive = 't', target_dead = 'f', check_obstacle = 'f'
  WHERE id IN (17737, 17738);

-- The server still rejects combat stuns and allows these skills under the anchor status.
UPDATE skills SET source_stun = 't'
  WHERE id IN (13608, 13609, 17649, 17949, 17950, 17957, 17958, 20704);

COMMIT;
'@)
Write-Output "Patched ship skills in $database"
