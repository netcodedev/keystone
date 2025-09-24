-- ------------------------------
-- OPTION
-- ------------------------------

OPTION IMPORT;

-- ------------------------------
-- TABLE: schema_lock
-- ------------------------------

DEFINE TABLE schema_lock TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;

DEFINE FIELD OVERWRITE instance_id ON schema_lock TYPE option<string> DEFAULT NONE PERMISSIONS FULL;
DEFINE FIELD OVERWRITE locked ON schema_lock TYPE bool DEFAULT false ASSERT IF $value = false OR $value = NONE { RETURN true; } ELSE { IF $value = true AND instance_id != NONE { RETURN true; } ELSE { THROW 'instance_id must be set when locking'; }; } PERMISSIONS FULL;
DEFINE FIELD OVERWRITE locked_at ON schema_lock TYPE option<datetime> DEFAULT NONE PERMISSIONS FULL;

DEFINE EVENT OVERWRITE enforce_singleton ON schema_lock WHEN $before = NONE AND $after.id != schema_lock:singleton THEN { DELETE $after.id; };
DEFINE EVENT OVERWRITE lock_date ON schema_lock WHEN $before.locked = false AND $after.locked = true THEN { UPDATE schema_lock:singleton SET locked_at = time::now(); };
DEFINE EVENT OVERWRITE unlock_date ON schema_lock WHEN $before.locked = true AND $after.locked = false THEN { UPDATE schema_lock:singleton SET locked_at = NONE, instance_id = NONE; };

CREATE schema_lock:singleton CONTENT { locked: false };

-- ------------------------------
-- TABLE: schema_migration
-- ------------------------------

DEFINE TABLE schema_migration TYPE NORMAL SCHEMAFULL PERMISSIONS NONE;

DEFINE FIELD OVERWRITE migration ON schema_migration TYPE string PERMISSIONS FULL;
DEFINE FIELD OVERWRITE applied_at ON schema_migration TYPE datetime DEFAULT time::now() PERMISSIONS FULL;