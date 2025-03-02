// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package com.risingwave.connector.source.common;

import com.risingwave.connector.api.TableSchema;
import com.risingwave.connector.api.source.SourceTypeE;
import com.risingwave.proto.Data;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.SQLException;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;

public class MySqlValidator extends DatabaseValidator implements AutoCloseable {
    private final Map<String, String> userProps;

    private final TableSchema tableSchema;

    private final Connection jdbcConnection;

    public MySqlValidator(Map<String, String> userProps, TableSchema tableSchema)
            throws SQLException {
        this.userProps = userProps;
        this.tableSchema = tableSchema;

        var dbHost = userProps.get(DbzConnectorConfig.HOST);
        var dbPort = userProps.get(DbzConnectorConfig.PORT);
        var dbName = userProps.get(DbzConnectorConfig.DB_NAME);
        var jdbcUrl = ValidatorUtils.getJdbcUrl(SourceTypeE.MYSQL, dbHost, dbPort, dbName);

        var user = userProps.get(DbzConnectorConfig.USER);
        var password = userProps.get(DbzConnectorConfig.PASSWORD);
        this.jdbcConnection = DriverManager.getConnection(jdbcUrl, user, password);
    }

    @Override
    public void validateDbConfig() {
        try {
            // TODO: check database server version
            validateBinlogConfig();
        } catch (SQLException e) {
            throw ValidatorUtils.internalError(e.getMessage());
        }
    }

    private void validateBinlogConfig() throws SQLException {
        // check whether source db has enabled binlog
        try (var stmt = jdbcConnection.createStatement()) {
            var res = stmt.executeQuery(ValidatorUtils.getSql("mysql.bin_log"));
            while (res.next()) {
                if (!res.getString(2).equalsIgnoreCase("ON")) {
                    throw ValidatorUtils.internalError(
                            "MySQL doesn't enable binlog.\nPlease set the value of log_bin to 'ON' and restart your MySQL server.");
                }
            }
        }

        // check binlog format
        try (var stmt = jdbcConnection.createStatement()) {
            var res = stmt.executeQuery(ValidatorUtils.getSql("mysql.bin_format"));
            while (res.next()) {
                if (!res.getString(2).equalsIgnoreCase("ROW")) {
                    throw ValidatorUtils.internalError(
                            "MySQL binlog_format should be 'ROW'.\nPlease modify the config and restart your MySQL server.");
                }
            }
        }

        // check binlog image
        try (var stmt = jdbcConnection.createStatement()) {
            var res = stmt.executeQuery(ValidatorUtils.getSql("mysql.bin_row_image"));
            while (res.next()) {
                if (!res.getString(2).equalsIgnoreCase("FULL")) {
                    throw ValidatorUtils.internalError(
                            "MySQL binlog_row_image should be 'FULL'.\\nPlease modify the config and restart your MySQL server.");
                }
            }
        }
    }

    @Override
    public void validateUserPrivilege() {
        try {
            validatePrivileges();
        } catch (SQLException e) {
            throw ValidatorUtils.internalError(e.getMessage());
        }
    }

    @Override
    public void validateTable() {
        try {
            validateTableSchema();
        } catch (SQLException e) {
            throw ValidatorUtils.internalError(e.getMessage());
        }
    }

    private void validateTableSchema() throws SQLException {
        // check whether table exist
        try (var stmt = jdbcConnection.prepareStatement(ValidatorUtils.getSql("mysql.table"))) {
            stmt.setString(1, userProps.get(DbzConnectorConfig.DB_NAME));
            stmt.setString(2, userProps.get(DbzConnectorConfig.TABLE_NAME));
            var res = stmt.executeQuery();
            while (res.next()) {
                var ret = res.getInt(1);
                if (ret == 0) {
                    throw ValidatorUtils.invalidArgument("MySQL table doesn't exist");
                }
            }
        }

        // check whether PK constraint match source table definition
        try (var stmt =
                jdbcConnection.prepareStatement(ValidatorUtils.getSql("mysql.table_schema"))) {
            stmt.setString(1, userProps.get(DbzConnectorConfig.DB_NAME));
            stmt.setString(2, userProps.get(DbzConnectorConfig.TABLE_NAME));

            // Field name in lower case -> data type
            var schema = new HashMap<String, String>();
            var pkFields = new HashSet<String>();
            var res = stmt.executeQuery();
            while (res.next()) {
                var field = res.getString(1);
                var dataType = res.getString(2);
                var key = res.getString(3);
                schema.put(field.toLowerCase(), dataType);
                if (key.equalsIgnoreCase("PRI")) {
                    // RisingWave always use lower case for column name
                    pkFields.add(field.toLowerCase());
                }
            }

            // All columns defined must exist in upstream database
            for (var e : tableSchema.getColumnTypes().entrySet()) {
                // skip validate internal columns
                if (e.getKey().startsWith(ValidatorUtils.INTERNAL_COLUMN_PREFIX)) {
                    continue;
                }
                var dataType = schema.get(e.getKey().toLowerCase());
                if (dataType == null) {
                    throw ValidatorUtils.invalidArgument(
                            "Column '" + e.getKey() + "' not found in the upstream database");
                }
                if (!isDataTypeCompatible(dataType, e.getValue())) {
                    throw ValidatorUtils.invalidArgument(
                            "Incompatible data type of column " + e.getKey());
                }
            }

            if (!ValidatorUtils.isPrimaryKeyMatch(tableSchema, pkFields)) {
                throw ValidatorUtils.invalidArgument("Primary key mismatch");
            }
        }
    }

    private void validatePrivileges() throws SQLException {
        String[] privilegesRequired = {
            "SELECT", "RELOAD", "SHOW DATABASES", "REPLICATION SLAVE", "REPLICATION CLIENT",
        };

        var hashSet = new HashSet<>(List.of(privilegesRequired));
        try (var stmt = jdbcConnection.createStatement()) {
            var res = stmt.executeQuery(ValidatorUtils.getSql("mysql.grants"));
            while (res.next()) {
                String granted = res.getString(1).toUpperCase();
                // all privileges granted, check passed
                if (granted.contains("ALL")) {
                    break;
                }

                // remove granted privilege from the set
                hashSet.removeIf(granted::contains);
                if (hashSet.isEmpty()) {
                    break;
                }
            }
            if (!hashSet.isEmpty()) {
                throw ValidatorUtils.invalidArgument(
                        "MySQL user doesn't have enough privileges: " + hashSet);
            }
        }
    }

    @Override
    public void close() throws Exception {
        if (null != jdbcConnection) {
            jdbcConnection.close();
        }
    }

    private boolean isDataTypeCompatible(String mysqlDataType, Data.DataType.TypeName typeName) {
        int val = typeName.getNumber();
        switch (mysqlDataType) {
            case "tinyint": // boolean
                return (val == Data.DataType.TypeName.BOOLEAN_VALUE)
                        || (Data.DataType.TypeName.INT16_VALUE <= val
                                && val <= Data.DataType.TypeName.INT64_VALUE);
            case "smallint":
                return Data.DataType.TypeName.INT16_VALUE <= val
                        && val <= Data.DataType.TypeName.INT64_VALUE;
            case "mediumint":
            case "int":
                return Data.DataType.TypeName.INT32_VALUE <= val
                        && val <= Data.DataType.TypeName.INT64_VALUE;
            case "bigint":
                return val == Data.DataType.TypeName.INT64_VALUE;

            case "float":
            case "real":
                return val == Data.DataType.TypeName.FLOAT_VALUE
                        || val == Data.DataType.TypeName.DOUBLE_VALUE;
            case "double":
                return val == Data.DataType.TypeName.DOUBLE_VALUE;
            case "decimal":
                return val == Data.DataType.TypeName.DECIMAL_VALUE;
            case "varchar":
                return val == Data.DataType.TypeName.VARCHAR_VALUE;
            default:
                return true; // true for other uncovered types
        }
    }
}
