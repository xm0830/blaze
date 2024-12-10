/*
 * Copyright 2022 The Blaze Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
package org.apache.spark.sql.blaze

import org.apache.spark.SparkEnv
import org.apache.spark.internal.Logging
import org.apache.spark.internal.config.ConfigEntry
import org.apache.spark.sql.SparkSessionExtensions
import org.apache.spark.sql.catalyst.rules.Rule
import org.apache.spark.sql.execution.SparkPlan
import org.apache.spark.sql.SparkSession
import org.apache.spark.sql.execution.ColumnarRule
import org.apache.spark.sql.execution.LocalTableScanExec
import org.apache.spark.sql.internal.SQLConf

class BlazeSparkSessionExtension extends (SparkSessionExtensions => Unit) with Logging {
  Shims.get.initExtension()

  override def apply(extensions: SparkSessionExtensions): Unit = {
    SparkEnv.get.conf.set("spark.sql.adaptive.enabled", "true")
    SparkEnv.get.conf.set("spark.sql.adaptive.forceApply", "true")
    logInfo("org.apache.spark.BlazeSparkSessionExtension enabled")

    assert(BlazeSparkSessionExtension.blazeEnabledKey != null)
    Shims.get.onApplyingExtension(extensions)

    extensions.injectColumnar(sparkSession => {
      BlazeColumnarOverrides(sparkSession)
    })
  }
}

object BlazeSparkSessionExtension extends Logging {
  lazy val blazeEnabledKey: ConfigEntry[Boolean] = SQLConf
    .buildConf("spark.blaze.enable")
    .booleanConf
    .createWithDefault(true)

  def dumpSimpleSparkPlanTreeNode(exec: SparkPlan, depth: Int = 0): Unit = {
    val nodeName = exec.nodeName
    val convertible = exec
      .getTagValue(BlazeConvertStrategy.convertibleTag)
      .getOrElse(false)
    val strategy =
      exec.getTagValue(BlazeConvertStrategy.convertStrategyTag).getOrElse(Default)
    logInfo(s" +${"-" * depth} $nodeName (convertible=$convertible, strategy=$strategy)")
    exec.children.foreach(dumpSimpleSparkPlanTreeNode(_, depth + 1))
  }
}

case class BlazeColumnarOverrides(sparkSession: SparkSession) extends ColumnarRule with Logging {
  import BlazeSparkSessionExtension._

  override def preColumnarTransitions: Rule[SparkPlan] = {
    new Rule[SparkPlan] {
      override def apply(sparkPlan: SparkPlan): SparkPlan = {
        if (!sparkPlan.conf.getConf(blazeEnabledKey)) {
          return sparkPlan // performs no conversion if blaze is not enabled
        }

        if (sparkPlan.isInstanceOf[LocalTableScanExec]) {
          return sparkPlan // skip useless local table scan (generated by set, addjar, etc)
        }

        // generate convert strategy
        BlazeConvertStrategy.apply(sparkPlan)
        logInfo("Blaze convert strategy for current stage:")
        dumpSimpleSparkPlanTreeNode(sparkPlan)

        val sparkPlanTransformed = BlazeConverters.convertSparkPlanRecursively(sparkPlan)
        logInfo("Blaze convert result for current stage:")
        dumpSimpleSparkPlanTreeNode(sparkPlanTransformed)

        logInfo(s"Transformed spark plan after preColumnarTransitions:\n${sparkPlanTransformed
          .treeString(verbose = true, addSuffix = true)}")

        // post-transform
        Shims.get.postTransform(sparkPlanTransformed, sparkSession.sparkContext)
        sparkPlanTransformed
      }
    }
  }
}
